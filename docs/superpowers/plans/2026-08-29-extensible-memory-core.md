# Extensible Memory Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the provider-neutral Memory contracts, conservative policy/null implementations, and a real local SQLite/FTS5 Store/Retriever that later phases can wire into Otto without changing the agent loop.

**Architecture:** `internal/memory` owns stable domain types and narrow Store/Retriever/Extractor/Policy/Service interfaces. `internal/memory/sqlite` implements Store and Retriever behind a compile-time Factory using a shared SQLite file; this phase deliberately does not wire Memory into config, tools, the agent loop, TUI, REPL, backup, restore, or extraction.

**Tech Stack:** Go 1.26, `database/sql`, `modernc.org/sqlite v1.57.0` (pure Go, SQLite 3.53.3 with FTS5/JSON1), standard `testing`, real `t.TempDir()` databases.

**Spec:** `docs/superpowers/specs/2026-08-29-extensible-memory-design.md`

## Global Constraints

- Stage 1 provider support remains `openai-compatible`; this phase contains no provider-specific wire code.
- Work only in an isolated feature worktree created from the plan commit; never implement on `main` or the design worktree.
- Use strict TDD: add a failing test, run it and observe the expected failure, make the minimum implementation, rerun focused tests, then commit.
- Default tests must remain offline and require no credentials, Node/Pi installation, SQLite CLI, network, or interactive terminal.
- Do not modify Pi v3 session persistence, compaction, `RunOnce`, TUI, REPL, tool registration, config, README, or runtime logging in this phase.
- Do not add dynamic Go plugins, vectors/embeddings, remote backends, backup/restore, migration locks, or an LLM extractor.
- Never persist or echo credentials, OAuth tokens, authorization/cookie headers, URI userinfo, private keys, redacted placeholders, resolved provider endpoint/configuration values, prompts, or raw tool output.
- Public API results must deep-clone slices, maps, byte slices, and pointer fields; callers must not alias Store state.
- Never invoke caller-supplied `ContentGuard`, token estimator, clock, or ID callback while a SQL row/transaction/borrowed connection or lifecycle lock is held. Reads buffer only bounded projections, release SQL resources, then guard/callback; writes pre-guard snapshots before acquiring the write gate and verify a canonical digest/revision again inside the transaction before mutation.
- Durable record/base/result revisions are in `1..math.MaxInt64` (zero only where the API explicitly means “not yet persisted”); increment overflow is rejected before SQL mutation.
- Record text is capped at 8 KiB; namespace/kind at 32 bytes; record/candidate/observation IDs at 64; scope/session/message IDs at 128; semantic key at 256; 32 labels of 64 bytes; 32 metadata entries with 64-byte keys, 512-byte values, and 4 KiB total; reason at 2 KiB; provenance at 32 message IDs.
- Query and serialized observation inputs are capped at 8 KiB and 64 KiB; observations allow 32 message IDs and 32 tool facts, with 64-byte tool names and 2 KiB tool-fact text; FTS construction at 64 terms/256 bytes per term; baseline recall at 16 records and lexical candidate oversampling at 256 rows and future management candidate filtering at 500 scanned rows/page; request filters at 16 scopes/16 kinds/16 labels; pages at 100 entities; total recall at 64 records/8192 estimated tokens; candidate batches at 8/256 KiB canonical aggregate; guard inputs at 512 fields/64 KiB with at most 8192 exact-match candidate spans, and exact forbidden sets at 64 values/8 KiB each; decoded cursors at 4 KiB.
- SQLite directories use `0700`; database files use `0600`; symlinked final database paths are rejected.
- SQLite uses WAL, foreign keys, FULL synchronous mode, a 5-second busy timeout, immediate transactions, defensive mode, DQS disabled, parameterized SQL, and retained/configured physical connections with immutable SQLite resource limits.
- Required Store semantics are atomic and non-optional: conditional revisions, deterministic opaque pagination, observation idempotency, candidate review plus accepted mutation in one transaction, and exact field round-trip.
- Errors wrap stable sentinels and never include Record text, metadata values, query text, or proposal content.
- Keep files focused by responsibility; do not place the whole subsystem in one file.

---

## File map

### Neutral package

- Create `internal/memory/errors.go`: sentinel and typed errors.
- Create `internal/memory/types.go`: Scope, Record, Candidate, requests/results, capabilities, and constants.
- Create `internal/memory/interfaces.go`: TurnMemory, Manager, Proposer, Binding, Service, Store, Retriever, Extractor, Policy, Maintenance, and Factory.
- Create `internal/memory/limits.go`: immutable hard limits.
- Create `internal/memory/clone.go`: deep-clone helpers for all API values.
- Create `internal/memory/id.go`: cryptographically random opaque IDs with injectable generator seam.
- Create `internal/memory/scope.go`: user/workspace scope construction and canonical-path hashing.
- Create `internal/memory/validate.go`: domain and request validation.
- Create `internal/memory/guard.go`: generic secret/redaction-marker rejection.
- Create `internal/memory/policy.go`: conservative default policy.
- Create `internal/memory/null.go`: disabled/unavailable Service and Binding.
- Create `internal/memory/memorytest/store.go`: reusable Store conformance harness.
- Create focused `_test.go` files beside each responsibility.

### SQLite adapter

- Create `internal/memory/sqlite/open.go`: secure path setup, DSN, pool, pragmas, close, and identity.
- Create `internal/memory/sqlite/schema.go`: schema v1 and schema verification.
- Create `internal/memory/sqlite/tx.go`: serialized immediate write transactions and SQLite error mapping.
- Create `internal/memory/sqlite/codec.go`: deterministic JSON/time/domain row encoding.
- Create `internal/memory/sqlite/cursor.go`: versioned opaque cursors.
- Create `internal/memory/sqlite/records.go`: Get/List/ListTombstones/Upsert/Forget.
- Create `internal/memory/sqlite/candidates.go`: Propose/CommitObservation/ListCandidates/Review.
- Create `internal/memory/sqlite/query.go`: safe FTS term construction and query filters.
- Create `internal/memory/sqlite/retriever.go`: baseline plus FTS retrieval, precedence, dedupe, and budgets.
- Create `internal/memory/sqlite/factory.go`: compile-time Factory and unsupported Maintenance adapter.
- Create focused adapter tests plus conformance and concurrency tests.
- Modify `go.mod` and `go.sum`: add exactly `modernc.org/sqlite v1.57.0` and its resolved indirect dependencies.

## Locked Phase 1 API shape

Task 1 defines these fields and names exactly. Later tasks may add private helpers but must not rename or duplicate them:

```go
type Scope struct { Namespace, ID string }

type Provenance struct {
    Origin         Origin
    SessionID      string
    MessageIDs     []string
    ObservationID  string
    DecisionAt     *time.Time
    DecisionSource Origin
}

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

type Tombstone struct {
    ID          string
    Scope       Scope
    Revision    uint64
    CreatedAt   time.Time
    UpdatedAt   time.Time
    ForgottenAt time.Time
}

type Candidate struct {
    ID             string
    Proposed       Record
    Action         CandidateAction
    TargetID       string
    BaseRevision   uint64
    Reason         string
    State          CandidateState
    CreatedAt      time.Time
    DecidedAt      *time.Time
    DecisionSource Origin
    ResultRecordID string
    ResultRevision uint64
}

type RecordRef struct { Scope Scope; ID string }
type RecordKey struct { Scope Scope; Kind, Key string }
type CandidateRef struct { Scope Scope; ID string }
type StoreIdentity struct { DatabaseID string; UserScope Scope; SchemaVersion int; Generation uint64 }
type RecordPage struct { Records []Record; NextCursor string }
type TombstonePage struct { Tombstones []Tombstone; NextCursor string }
type CandidatePage struct { Candidates []Candidate; NextCursor string }
type CandidateBatch struct { Candidates []Candidate }

type ListRequest struct {
    Scopes []Scope
    Kinds, Labels []string
    Limit int
    Cursor string
    Now time.Time
    IncludeExpired bool
}

type TombstoneListRequest struct { Scopes []Scope; Limit int; Cursor string }
type CandidateListRequest struct { Scopes []Scope; States []CandidateState; Limit int; Cursor string }
type UpsertRequest struct { Record Record; ExpectedRevision *uint64 }
type StoreForgetRequest struct { Ref RecordRef; ExpectedRevision uint64; ForgottenAt time.Time }
type ProposalBatch struct { Candidates []Candidate }
type ObservationCommit struct { ObservationID string; Candidates []Candidate; CreatedAt time.Time }
type ObservationReceipt struct { ObservationID string; CandidateIDs []string; Existing bool }
type StoreReviewRequest struct { Ref CandidateRef; ResultRecordID string; Decision ReviewDecision; Edited *Record; TargetRevision *uint64; DecisionSource Origin; DecidedAt time.Time }
type ReviewResult struct { Candidate Candidate; Record *Record; Tombstone *Tombstone }

type RetrievalRequest struct {
    Query string
    Scopes []Scope
    Kinds, Labels []string
    IncludeExpired, IncludeBaseline bool
    Limit, TokenBudget int
    Now time.Time
    EstimateTokens func(string) int
    Cursor string
}
type RetrievalMatch struct { Record Record; Rank int }
type RetrievalResult struct { Matches []RetrievalMatch; UsedTokens int; NextCursor string }

type RecallRequest struct { Query string; Kinds []string; Limit, TokenBudget int }
type RecallResult struct { Records []Record; UsedTokens int }
type ToolFact struct { ToolName, Text string }
type Observation struct {
    ID, UserText, AssistantText, SessionID string
    ToolFacts []ToolFact
    MessageIDs []string
}
type ObserveResult struct { CandidateIDs []string; Existing bool }

type SearchRequest struct { Query string; Scopes []Scope; Kinds, Labels []string; IncludeExpired, IncludeCandidates bool; CandidateStates []CandidateState; Limit, TokenBudget int; Cursor string; Now time.Time }
type SearchResult struct { Records []Record; Candidates []Candidate; NextCursor string }
type RememberRequest struct { ID string; Scope Scope; Kind, Key, Text string; Labels []string; Metadata map[string]string; Confidence float64; ExpiresAt *time.Time; ExpectedRevision *uint64; Source Provenance }
type ForgetRequest struct { Ref RecordRef; ExpectedRevision uint64; PurgeBackups, ConfirmPurge bool }
type ForgetResult struct { Tombstone Tombstone; PurgedBackupIDs []string }
type ProposeRequest struct { Action CandidateAction; Scope Scope; Kind, Key, Text string; Labels []string; Metadata map[string]string; Confidence float64; ExpiresAt *time.Time; TargetID string; BaseRevision uint64; Reason string; Source Provenance }
type ReviewRequest struct { Ref CandidateRef; Decision ReviewDecision; Edited *Record; TargetRevision *uint64 }

type ExtractRequest struct { Observation Observation; Existing []Record }
type Proposal struct { Action CandidateAction; Scope Scope; Kind, Key, Text string; Labels []string; Metadata map[string]string; Confidence float64; ExpiresAt *time.Time; TargetID string; BaseRevision uint64; Reason string }
type PolicyRequest struct { Origin Origin; Action CandidateAction; Scope Scope; Kind string; Confidence float64; Source Provenance; Valid, Sensitive bool }
type PolicyDecision string

type GuardField struct { Name, Value string; Opaque bool }
type GuardInput struct { Fields []GuardField }
type BindOptions struct { Scopes []Scope; DefaultWriteScope Scope; Extractor Extractor; Guard ContentGuard; EstimateTokens func(string) int; Now func() time.Time }
type Capabilities struct { LexicalSearch, SemanticSearch, OnlineBackup, EncryptionAtRest, ConcurrentProcesses bool }
type Components struct { Store Store; Retriever Retriever; Maintenance Maintenance; Capabilities Capabilities }

type PurgeForgottenRequest struct { Tombstone Tombstone; Confirm bool }
type PurgeForgottenResult struct { PurgedBackupIDs []string }
type BackupRequest struct { Class string }
type BackupInfo struct { ID string; CreatedAt time.Time; SchemaVersion int; DatabaseID, Class, DatabaseSHA256, LedgerSHA256 string; Bytes int64 }
type RestoreRequest struct { Backup string; AllowMigration, Confirm bool }
```

RetrievalMatch.Rank is a final one-based ordinal assigned after merge/dedupe, never a backend score; RetrievalResult.UsedTokens is the exact saturated sum for included whole records. `ValidateRecord` validates an active persisted Record and therefore requires ID, revision, and timestamps. `ValidateProposedRecord` validates candidate/remember content before persistence and requires empty ID, revision zero, and zero CreatedAt/UpdatedAt; the service/store sets durable identity and times. `ValidateUpsertRequest` requires a create Record with revision zero when `ExpectedRevision` is nil, or an update Record whose revision equals the nonzero expected revision. `ValidateCandidate` applies action/state-specific proposed validation; decided candidates may carry a content-cleared Proposed value.

---

### Task 1: Lock the neutral Memory contracts

**Files:**
- Create: `internal/memory/errors.go`
- Create: `internal/memory/types.go`
- Create: `internal/memory/interfaces.go`
- Create: `internal/memory/limits.go`
- Create: `internal/memory/clone.go`
- Create: `internal/memory/id.go`
- Create: `internal/memory/scope.go`
- Test: `internal/memory/types_test.go`
- Test: `internal/memory/clone_test.go`
- Test: `internal/memory/scope_test.go`

**Interfaces:**
- Consumes: only the Go standard library.
- Produces: every neutral type and interface used by all later tasks. Later tasks must use these exact names rather than defining adapter-local substitutes.

- [ ] **Step 1: Write compile-time and literal contract tests**

Create tests that reference the final contracts before they exist. Include compile-time assertions with test fakes and literal expectations for constants, deep-copy behavior, and scope IDs:

```go
func TestWorkspaceScopeUsesStableOverrideOrPathDigest(t *testing.T) {
    workspace := t.TempDir()
    physical, err := filepath.EvalSymlinks(workspace)
    if err != nil { t.Fatal(err) }
    got, err := NewWorkspaceScope(workspace, "")
    if err != nil { t.Fatal(err) }
    sum := sha256.Sum256([]byte(physical))
    want := Scope{Namespace: NamespaceWorkspace, ID: "sha256:" + hex.EncodeToString(sum[:])}
    if got != want { t.Fatalf("scope = %#v, want %#v", got, want) }

    got, err = NewWorkspaceScope(workspace, "stable-project")
    if err != nil { t.Fatal(err) }
    if want := (Scope{Namespace: NamespaceWorkspace, ID: "stable-project"}); got != want {
        t.Fatalf("override scope = %#v, want %#v", got, want)
    }
}

func TestCloneRecordDoesNotAlias(t *testing.T) {
    expiry := time.Date(2026, 8, 29, 1, 2, 3, 0, time.UTC)
    original := Record{Labels: []string{"one"}, Metadata: map[string]string{"k": "v"}, ExpiresAt: &expiry,
        Source: Provenance{MessageIDs: []string{"m1"}}}
    cloned := CloneRecord(original)
    cloned.Labels[0], cloned.Metadata["k"], cloned.Source.MessageIDs[0] = "two", "changed", "m2"
    *cloned.ExpiresAt = expiry.Add(time.Hour)
    if original.Labels[0] != "one" || original.Metadata["k"] != "v" || original.Source.MessageIDs[0] != "m1" || !original.ExpiresAt.Equal(expiry) {
        t.Fatalf("clone aliases original: original=%#v clone=%#v", original, cloned)
    }
}
```

Also assert every listed sentinel is distinct, `errors.Is(&ConflictError{...}, ErrConflict)`, a valid `NewCommitUnknownError` unwraps to `ErrCommitUnknown` and clones its IDs, `NewID()` returns 32 lowercase hexadecimal characters, two generated IDs differ, and `Reader`, `Service`, `Store`, `Retriever`, `Extractor`, `Policy`, and `Maintenance` can be implemented without importing an adapter package.

- [ ] **Step 2: Run the contract tests and observe RED**

Run:

```bash
go test ./internal/memory -run 'Test(WorkspaceScope|CloneRecord|NewID|Errors|Interface)' -count=1
```

Expected: compile failure because package types/functions do not exist.

- [ ] **Step 3: Add sentinels and safe typed errors**

Define these sentinels in `errors.go`:

```go
var (
    ErrDisabled           = errors.New("memory is disabled")
    ErrUnavailable        = errors.New("memory is unavailable")
    ErrConflict           = errors.New("memory revision conflict")
    ErrSensitiveMemory    = errors.New("memory contains sensitive data")
    ErrUnsupported        = errors.New("memory operation is unsupported")
    ErrMemoryInUse        = errors.New("memory is in use")
    ErrPersistenceDisabled = errors.New("memory persistence is disabled")
    ErrBusy               = errors.New("memory store is busy")
    ErrCommitUnknown      = errors.New("memory commit outcome is unknown")
    ErrCorrupt            = errors.New("memory data is corrupt")
    ErrIncompatibleSchema = errors.New("memory schema is incompatible")
    ErrInvalidRecord      = errors.New("invalid memory record")
    ErrInvalidRequest     = errors.New("invalid memory request")
    ErrNotFound           = errors.New("memory entity not found")
    ErrClosed             = errors.New("memory is closed")
    ErrInvalidCursor      = errors.New("invalid memory cursor")
    ErrIncompleteForget   = errors.New("memory was forgotten but tombstone recording is incomplete")
    ErrIncompletePurge    = errors.New("memory backup purge is incomplete")
)
```

`ConflictError` contains only entity kind, opaque ID, expected revision, and actual revision; its `Unwrap()` returns `ErrConflict`. `CommitUnknownError` has private fields, is created only through `NewCommitUnknownError(CommitOperation, []string) (*CommitUnknownError, error)`, which validates a closed operation enum and at most 16 opaque IDs before cloning them, returns cloned IDs from accessors, contains only operation and bounded opaque entity IDs, unwraps `ErrCommitUnknown`, and directs callers to reconcile through Reader/Get/ListCandidates before retry. Add equivalent safe detail wrappers only where a caller needs structured recovery; never retain content in an error.

- [ ] **Step 4: Add the domain types and constants**

Define typed string constants for namespaces (`user`, `workspace`), origins (`human`, `model`, `extractor`, `import`, `migration`), candidate actions (`create`, `update`, `forget`), candidate states (`pending`, `accepted`, `rejected`), review decisions (`accept`, `reject`), commit operations (`schema`, `upsert`, `forget`, `propose`, `observe`, `review`), and record states used by Store filtering (`active`, `tombstone`).

Implement every Record/Candidate/request/result declaration exactly once from the canonical locked API block above; do not maintain a second abbreviated declaration list. No neutral type contains a backend-specific score or SQL field. Add typed constants `PolicyAccept`, `PolicyPending`, and `PolicyReject` for `PolicyDecision`.

- [ ] **Step 5: Add exact interfaces**

Implement these complete service/component interfaces. Keep RecallRequest/RecallResult as distinct named Agent-facing structs: Binding injects its immutable allowed Scopes and estimator into the internal RetrievalRequest, validates automatic proposals against those Scopes, and Agent never chooses scope identities/time or receives RetrievalMatch rank/backend scoring data. BindOptions requires one user Scope, at most one workspace Scope, and a DefaultWriteScope contained in Scopes.

```go
type TurnMemory interface {
    Recall(context.Context, RecallRequest) (RecallResult, error)
    Observe(context.Context, Observation) (ObserveResult, error)
}
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
type Proposer interface { Propose(context.Context, ProposeRequest) (CandidateBatch, error) }
type Binding interface { TurnMemory; Close() error }
type Service interface { Manager; Proposer; Bind(context.Context, BindOptions) (Binding, error); Close() error }
type Retriever interface { Retrieve(context.Context, RetrievalRequest) (RetrievalResult, error) }
type Extractor interface { Extract(context.Context, ExtractRequest) ([]Proposal, error) }
type Policy interface { Decide(context.Context, PolicyRequest) (PolicyDecision, error) }
type Maintenance interface {
    Backup(context.Context, BackupRequest) (BackupInfo, error)
    ListBackups(context.Context) ([]BackupInfo, error)
    VerifyBackup(context.Context, string) (BackupInfo, error)
    Restore(context.Context, RestoreRequest) error
    PurgeForgotten(context.Context, PurgeForgottenRequest) (PurgeForgottenResult, error)
}
type ContentGuard interface { Check(context.Context, GuardInput) error }
type Factory interface { Open(context.Context) (Components, error) }

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
```

`Service` embeds `Manager` and `Proposer`, creates immutable `Binding` values with `Bind`, and owns shared close. Reader exact methods are the only app/frontend path for CAS refresh/reconciliation; neither app nor tools receive Store. Retriever pagination is neutral and opaque. The future core Service encodes a versioned composite Search cursor containing only the Store.List-or-Retriever cursor, candidate cursor, phase, and request fingerprint. For empty management queries it uses Store.List (supporting complete active/expired listing); for textual queries it uses Retriever with IncludeBaseline=false and the requested IncludeExpired value. It applies the fixed conservative byte estimator (`0` for empty, otherwise `1 + (bytes-1)/3`) and SearchRequest.TokenBudget to whole results, returns records first, and uses remaining Limit/budget for Store.ListCandidates pages with bounded candidate text filtering; its deterministic whole-result budget strings mirror Retriever records and include candidate ID/action/scope/reason/proposed text for pending candidates. Turn recall instead calls Retriever with IncludeBaseline=true, IncludeExpired=false, and Binding's runtime estimator. This makes the full management Search contract implementable without adapter access. `Binding` embeds `TurnMemory` and has `Close() error`.

- [ ] **Step 6: Add IDs, scopes, hard limits, and cloning**

Use 16 random bytes encoded as 32 lowercase hexadecimal characters for IDs. Callers that require multiple distinct generated IDs retry a duplicate at most eight times, then fail with a safe category; tests inject duplicate sequences. `NewWorkspaceScope` hashes the already canonical path exactly as supplied unless a stable override is non-empty. `NewUserScope` validates an opaque installation ID. Do not persist or return the raw workspace path.

Deep-clone every nested API value, including candidate proposed records, edited records, page slices, metadata maps, labels, message IDs, candidate IDs, and time pointers. `nil` remains `nil`; empty non-nil collections remain independent non-nil collections.

- [ ] **Step 7: Run focused and package tests**

Run:

```bash
go test ./internal/memory -count=1
go test -race ./internal/memory -count=1
```

Expected: PASS.

- [ ] **Step 8: Commit the neutral contracts**

```bash
git add internal/memory
git commit -m "feat: define extensible memory contracts"
```

---

### Task 2: Enforce validation and secret rejection

**Files:**
- Create: `internal/memory/validate.go`
- Create: `internal/memory/guard.go`
- Test: `internal/memory/validate_test.go`
- Test: `internal/memory/guard_test.go`

**Interfaces:**
- Consumes: Task 1 domain types and hard limits.
- Produces: `ValidateScope`, `ValidateRecord`, `ValidateCandidate`, request validators, `ContentGuard`, `DefaultGuard`, hash-only `ExactGuard`, `CompositeGuard`, `GuardRecord`, `GuardCandidate`, `GuardObservationCommit`, and `GuardObservationReceipt`, used by the Service and SQLite adapter.

- [ ] **Step 1: Write table-driven validation tests**

Cover every exact byte/count boundary and one-over-boundary case. Include invalid UTF-8, control characters in IDs/keys/labels, non-finite confidence, zero/non-UTC timestamps, expiry before creation, duplicate normalized labels, unknown enum values, update/forget candidate without target/base revision, create candidate with a target, and invalid page limits/cursors. Assert validation never mutates caller-owned slices or maps.

Literal examples:

```go
func TestValidateRecordRejectsOneBytePastTextLimit(t *testing.T) {
    record := validRecord()
    record.Text = strings.Repeat("x", MaxRecordTextBytes+1)
    err := ValidateRecord(record)
    if !errors.Is(err, ErrInvalidRecord) || strings.Contains(fmt.Sprint(err), record.Text) {
        t.Fatalf("error = %v, want safe ErrInvalidRecord", err)
    }
}

func TestValidateCandidateRequiresTargetRevisionForUpdate(t *testing.T) {
    candidate := validCandidate()
    candidate.Action = CandidateUpdate
    candidate.TargetID = ""
    candidate.BaseRevision = 0
    if err := ValidateCandidate(candidate); !errors.Is(err, ErrInvalidRecord) {
        t.Fatalf("error = %v, want ErrInvalidRecord", err)
    }
}
```

- [ ] **Step 2: Write secret-guard tests**

Use redacted samples only. Assert rejection without echo for:

- `Authorization: Bearer [REDACTED]`
- `Cookie: session=[REDACTED]`
- `https://user:[REDACTED]@example.invalid/path`
- PEM private-key delimiters
- common key/token assignment shapes with `[REDACTED]`
- the exact `[REDACTED]` marker in text, key, label, metadata, or Candidate reason

Also assert ordinary code, URLs without userinfo, metadata such as `token_budget=2000`, and prose containing the word “tokenization” pass. A one-over-limit GuardInput fails safely before scanning. ExactGuard hashes synthetic configured-secret/resolved-endpoint values at construction, retains no raw value, scans opaque and semantic fields, rejects whole-field and bounded lexical/header/URI-span exact matches (including a synthetic secret embedded in prose) without echo, and `NewCompositeGuard` copies its member list into an immutable, concurrency-safe CompositeGuard that fails closed if either member fails.

- [ ] **Step 3: Run focused tests and observe RED**

```bash
go test ./internal/memory -run 'TestValidate|Test.*Guard' -count=1
```

Expected: compile failure for missing validators and guard.

- [ ] **Step 4: Implement validators with safe field-only errors**

Validation errors use field names and limit values, never rejected values. Namespace and Kind must match lowercase ASCII `^[a-z][a-z0-9._-]*$`; opaque IDs must match `[A-Za-z0-9._:-]+`; other user text may be Unicode but must be valid UTF-8, already trimmed where semantic identity requires it, and free of NUL/C0/C1 controls. Reject NaN/infinite confidence and require `0..1`. Validate UTF-8 before byte-size checks, require UTC normalized timestamps without monotonic readings and with years 0001–9999, sort neither caller slices nor maps in place, and reject case-fold duplicate labels or metadata keys after trimmed lowercase comparison. Semantic keys and label filters are trimmed, exact, and case-sensitive; Store never silently normalizes them.

High-level Search/Retrieval validators require at least one Scope; Store List/ListTombstones/ListCandidates with an empty scope set defensively return an empty page (never all data), while empty kind/label/state filters mean “all allowed values.” SearchRequest requires CandidateStates empty when IncludeCandidates is false; when true, an empty state filter means all candidate states and SearchResult uses one future composite opaque cursor. Limits must be positive and at or below their hard ceiling (Store/Retriever pages 100; future TurnMemory Recall additionally clamps to 64), request `Now` must be nonzero UTC when expiry is evaluated (`ListRequest.IncludeExpired` may leave it zero), and token budgets must be positive and at or below 8192. Phase 1 retrieval rejects custom namespaces and accepts exactly one `user` Scope plus at most one `workspace` Scope, avoiding undefined precedence while Store paging remains namespace-extensible. BindOptions has the same Phase 1 namespace rule, no duplicates, and a DefaultWriteScope contained in Scopes; nil Extractor means automatic extraction disabled, while nil Guard/EstimateTokens/Now resolve to safe defaults. Injected Bind callbacks must be concurrency-safe and non-reentrant. Binding injects a normalized UTC Now into every retrieval, so Agent cannot make expired memory eligible; it maps logical user/workspace choices to its immutable Scope values, fills an omitted proposal scope from DefaultWriteScope, and rejects any supplied Scope not exactly in the binding. ForgetRequest requires ConfirmPurge when PurgeBackups is true; the future neutral Service durably forgets live data first, then calls Maintenance.PurgeForgotten with the content-free Tombstone, returning the Tombstone plus `ErrIncompletePurge` on partial backup failure so retry is idempotent. RememberRequest provenance is either zero (Service fills `human`) or already `human`/`migration`; model/extractor/import origins are rejected on the Manager path. Remember creates require empty ID and nil expected revision; revision-checked updates require a nonzero expected revision plus an ID, a non-empty semantic key, or both (both must identify the same record at Service time).

ProposeRequest accepts only model/extractor/import provenance; human writes use Manager and migration uses the internal migration path.

Provenance requires DecisionAt and DecisionSource to be both zero or both set; active records originating from model/extractor/import require a nonzero decision by human/migration, while pending proposed content has no decision metadata. DecisionAt is bounded UTC and cannot follow the materialized Record UpdatedAt.

Candidate rules:

- all pending Candidates have model/extractor/import Source origin; human/migration content is never routed through candidate creation.
- create: no target ID/base revision; proposed ID/timestamps are empty/zero and revision is zero before persistence.
- update: target ID and base revision greater than zero; proposed ID/timestamps are empty/zero and revision is zero.
- forget: target ID and base revision greater than zero; Proposed retains only Scope and pending Source provenance, with ID/kind/key/text/labels/metadata/confidence/expiry/revision/timestamps empty.
- pending: no decision time/source/result; accepted/rejected: decision time not before CreatedAt plus consistent decision/result fields, empty reason, and Proposed retains only Scope (all other fields zero/empty).
- Store Review decision source is limited to `human` or internal `migration`; model/extractor/import cannot approve Candidates. Review `Edited` is optional replacement proposed content (empty ID/revision/timestamps), allowed only when accepting create/update, and must retain the Candidate Scope; reject and accept-forget require nil Edited. ResultRecordID is a caller-generated valid ID required only for accepted create, keeping domain identity above every backend. TargetRevision is nil for create/reject, otherwise defaults to Candidate.BaseRevision; an explicit human/migration TargetRevision may rebase update/forget only after the caller refreshed the active Record. An update rebase where TargetRevision differs from Candidate.BaseRevision requires non-nil Edited content built from that refreshed Record; stale full proposals can never be blindly replayed.

- [ ] **Step 5: Implement `DefaultGuard`**

`ContentGuard` is:

```go
type ContentGuard interface {
    Check(context.Context, GuardInput) error
}
```

`GuardRecord` builds a bounded GuardInput covering every persisted caller-controlled string: semantic fields/metadata are ordinary values and record/candidate/scope/session/message IDs are marked Opaque. DefaultGuard skips token-shape heuristics for Opaque fields but still rejects explicit markers/URI userinfo; ExactGuard compares constant-time SHA-256+length fingerprints across whole fields and bounded lexical/header/URI candidate spans; span-count overflow fails closed before unbounded work. `GuardCandidate` adds the human/model-controlled reason. ContentGuard implementations must be concurrency-safe and must not retain GuardInput, reenter/close Memory components, or return errors containing any input value; token estimators and ID generators must also be concurrency-safe and obey the same no-reentry rule. CompositeGuard preserves context cancellation/deadline plus `ErrSensitiveMemory`/`ErrUnavailable`, but sanitizes unknown member errors to `ErrUnavailable` rather than wrapping arbitrary text. `DefaultGuard` scans every value using bounded, anchored patterns and URI parsing. It returns `ErrSensitiveMemory` with a safe category, never the matched bytes. It must reject explicit redaction markers, authorization/cookie header names with values, private-key block delimiters, URI userinfo, and conservative credential assignment forms. It must not claim to detect every possible secret. SQLite Options requires a non-nil composite guard, so exact matching is injectable in Phase 1 even before config/runtime wiring; ordinary URLs remain valid unless they exactly match a forbidden resolved value.

- [ ] **Step 6: Rerun tests and mutation-strength checks**

```bash
go test ./internal/memory -run 'TestValidate|Test.*Guard' -count=100
go test -race ./internal/memory -count=1
```

Temporarily weaken one upper-bound comparison locally and confirm the literal one-over-limit test fails; restore the implementation before committing.

- [ ] **Step 7: Commit validation and guard behavior**

```bash
git add internal/memory/validate.go internal/memory/validate_test.go internal/memory/guard.go internal/memory/guard_test.go
git commit -m "feat: validate durable memory content"
```

---

### Task 3: Add conservative policy and NullService

**Files:**
- Create: `internal/memory/policy.go`
- Create: `internal/memory/null.go`
- Test: `internal/memory/policy_test.go`
- Test: `internal/memory/null_test.go`

**Interfaces:**
- Consumes: Task 1 interfaces and Task 2 validation/guard.
- Produces: `DefaultPolicy`, `NewNullService(reason error) Service`, and a no-op Binding suitable for disabled/unavailable runtime degradation.

- [ ] **Step 1: Write policy authority tests**

Assert exact decisions:

```go
func TestDefaultPolicyAuthorityMatrix(t *testing.T) {
    tests := []struct{ origin Origin; sensitive, valid bool; want PolicyDecision }{
        {OriginHuman, false, true, PolicyAccept},
        {OriginModel, false, true, PolicyPending},
        {OriginExtractor, false, true, PolicyPending},
        {OriginImport, false, true, PolicyPending},
        {OriginMigration, false, true, PolicyAccept},
        {OriginHuman, true, true, PolicyReject},
        {OriginHuman, false, false, PolicyReject},
        {Origin("unknown"), false, true, PolicyReject},
    }
    for _, tt := range tests {
        got, err := (DefaultPolicy{}).Decide(context.Background(), PolicyRequest{
            Origin: tt.origin, Sensitive: tt.sensitive, Valid: tt.valid,
        })
        if err != nil { t.Fatalf("origin %q: %v", tt.origin, err) }
        if got != tt.want { t.Fatalf("origin %q: decision = %q, want %q", tt.origin, got, tt.want) }
    }
}
```

No policy request contains a model-controlled “confirmed” field.

- [ ] **Step 2: Write NullService lifecycle tests**

Assert:

- `Bind` rejects invalid BindOptions with `ErrInvalidRequest`; valid BindOptions succeed before Service close and return an independent Binding.
- Bound `Recall` and `Observe` return empty results and nil so normal turns can continue.
- `Get`, `GetByKey`, `GetTombstone`, `GetCandidate`, `Search`, `Remember`, `Forget`, `Review`, and `Propose` return a sanitized constructor category via `errors.Is` and never preserve arbitrary cause text.
- Closing one Binding does not affect another.
- Binding close and Service close are idempotent.
- Bind after Service close, calls through an already closed Binding, and calls through a Binding after parent Service close return `ErrClosed`.
- Results contain fresh non-aliased empty slices; an already canceled context returns `context.Canceled` before the disabled/unavailable category.

- [ ] **Step 3: Run tests and observe RED**

```bash
go test ./internal/memory -run 'TestDefaultPolicy|TestNullService' -count=1
```

Expected: compile failure for missing implementations.

- [ ] **Step 4: Implement `DefaultPolicy` and immutable decisions**

PolicyRequest carries bounded Action, Scope, Kind, Confidence, and Provenance (nonzero Source.Origin must equal Origin) so replacement/configured policies can apply scope/kind/source/confidence rules without backend access. `DefaultPolicy.Decide` checks context first, rejects invalid/sensitive requests, accepts `OriginHuman` and the internal `OriginMigration`, queues model/extractor/import origins, and rejects unknown origins. It returns no content-bearing error.

- [ ] **Step 5: Implement `NullService` with lock-safe close**

Use a mutex-protected Service closed flag; do not spawn goroutines. Normalize nil/disabled input to `ErrDisabled`, unavailable input to `ErrUnavailable`, and every other input to `ErrUnavailable` without retaining the original error string. Bindings hold the sanitized category, their own closed flag, and a parent closed-state reference; they hold no resources or callbacks. `Service.Close` marks the Service closed, invalidates future calls through existing Bindings, and returns without waiting because NullService owns no shared resource. Lock ordering is parent state then Binding state, and no lock is held while invoking caller code.

- [ ] **Step 6: Run focused/race tests and commit**

```bash
go test ./internal/memory -run 'TestDefaultPolicy|TestNullService' -count=100
go test -race ./internal/memory -count=1
git add internal/memory/policy.go internal/memory/policy_test.go internal/memory/null.go internal/memory/null_test.go
git commit -m "feat: add safe memory policy fallback"
```

---

### Task 4: Open a secure SQLite schema v1

**Files:**
- Modify: `go.mod`
- Modify: `go.sum`
- Create: `internal/memory/sqlite/open.go`
- Create: `internal/memory/sqlite/path_unix.go`
- Create: `internal/memory/sqlite/path_other.go`
- Create: `internal/memory/sqlite/schema.go`
- Create: `internal/memory/sqlite/tx.go`
- Test: `internal/memory/sqlite/open_test.go`
- Test: `internal/memory/sqlite/schema_test.go`

**Interfaces:**
- Consumes: `memory.StoreIdentity`, sentinels, and `NewID`.
- Produces: `Open(context.Context, string, Options) (*Store, error)`, SQLite `Store.Identity`, a schema-v1 database, and reusable write transaction/error helpers.

- [ ] **Step 1: Write SQLite open/schema tests before adding the driver**

Tests must use a real path under `t.TempDir()` and assert:

- Parent directory is created with mode `0700` and database with `0600`; after a write, any SQLite `-wal`/`-shm` sidecars have no group/world permission bits.
- Bootstrap sets persistent `journal_mode=wal` once, while each of four retained physical connections has foreign keys `1`, synchronous FULL, busy timeout exactly 5000 ms, trusted schema off, DQS fallback rejected, and defensive writable-schema protection; quick_check is `ok`.
- A canceled context before Open creates no database; cancellation detected after a non-cancelable driver open closes all resources and returns `context.Canceled`. Reopening a copied crash-style DB+WAL pair recovers committed data. A blocking NewID hook proves no SQL connection/transaction/lifecycle mutex is held during ID generation. Every retained connection reports every exact sqlite3_limit ceiling; Task 5 adds loaded-row projection tests once record reads exist.
- `memory_records_fts` exists and accepts a normal FTS insert/query, and `json_valid`/`json_each` execute, proving required FTS5/JSON support is compiled in; direct scalar/wrong-shape JSON, tombstone-with-content, action/target/base mismatch, proposed-scope mismatch, observation/source mismatch, and inconsistent candidate-state inserts fail schema/FK checks.
- Identity contains distinct 32-hex database/user IDs, user namespace is `user`, initial Generation is zero, and identity/generation survive close/reopen; a directly corrupted identity equal to a synthetic exact forbidden value fails reopen with ErrSensitiveMemory without echo.
- Existing parent symlinks are canonicalized to a physical parent, then descriptor-relative traversal holds a verified dedicated-parent handle and uses `openat`/`O_NOFOLLOW` for the final DB. Unsafe group/world-accessible or wrong-owner final parents, DB/WAL/SHM symlinks/nonregular files, and hook-driven ancestor/final-parent/DB/sidecar swaps before/after first connection and after sidecar creation fail; a special hook swaps only during `sqlite3_open`, restores before pathname recheck, and is still rejected by the connection-FD inode proof before schema mutation without changing a target. Existing group/world-accessible, non-regular, or non-current-user-owned databases are rejected without silently chmodding them (owner helper is unit-tested with synthetic metadata where tests cannot chown).
- A database with `PRAGMA user_version=2`, version zero plus an unknown user object, or version one with an added/dropped/changed table/index/FTS definition or schema fingerprint returns `ErrIncompatibleSchema`/`ErrCorrupt`; read-only preflight proves refused simple non-WAL future/unknown fixtures are byte-unchanged and create no sidecar; WAL-mode preflight may use a validated SQLite read-side SHM but performs no application/schema write.
- A channel-held external writer released through an event barrier before budget expiry lets BUSY retry succeed; holding it beyond a 10 ms test budget returns `ErrBusy` within a bounded interval. An injected primary `SQLITE_LOCKED` attempt and any BUSY after the transaction callback begins are not retried; extended BUSY codes classify by primary code, canceled context returns `context.Canceled`, and negative/overflowing timeout options return `ErrInvalidRequest`.
- Injected cancellation before commit rolls back; cancellation after the commit point cannot mask success; an injected driver-COMMIT failure returns content-free `ErrCommitUnknown`, poisons that handle to ErrUnavailable, and requires close/reopen reconciliation without leaking a retained connection; an injected rollback failure likewise discards/poisons and returns ErrUnavailable. Paths containing spaces, `?`, `#`, percent signs, and UTF-8 open the intended file through structured URI encoding. Every Open/path/schema refusal returns a safe category without the raw path or DSN; Close is idempotent and `Identity` after close returns `ErrClosed`. Task 9 extends lifecycle assertions.

- [ ] **Step 2: Run the new adapter tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestOpen|TestSchema|TestIdentity' -count=1
```

Expected: compile failure because adapter/driver do not exist.

- [ ] **Step 3: Add the exact SQLite dependency**

```bash
go list -m all > /tmp/otto-memory-modules.before
go get modernc.org/sqlite@v1.57.0
go mod tidy
go list -m all > /tmp/otto-memory-modules.after
go run honnef.co/go/tools/cmd/staticcheck@latest -version
GOPROXY="file://$(go env GOMODCACHE)/cache/download" GOSUMDB=off \
  go run honnef.co/go/tools/cmd/staticcheck@v0.8.1 -version
```

Inspect `git diff go.mod go.sum` and verify `modernc.org/sqlite v1.57.0` is direct and no unrelated direct dependency changed. From before/after `go list -m all`, derive every newly introduced module; inspect each local module-cache license, plus modernc SQLite's `LICENSE` and `LICENSE-SQLITE`, and record exact module/version/license identifiers in the task report. Also verify locally that JSON1/FTS5 and `modernc.org/sqlite.Limit` work with `CGO_ENABLED=0`. Also record the local license/version material for the pinned, cache-backed `honnef.co/go/tools v0.8.1` final verifier and its tool-only dependencies. Do not use network-backed license tooling or add a dependency whose local license cannot be verified.

- [ ] **Step 4: Implement secure path creation and DSN construction**

`Options` contains `BusyTimeout time.Duration`, `NewID func() (string, error)`, and required `Guard memory.ContentGuard`. Zero timeout resolves to five seconds; negative/overflowing millisecond values or nil Guard return a safe validation error. Tests construct `exact, err := memory.NewExactGuard(syntheticForbiddenValues)` (max 64 values/8 KiB each), then `guard, err := memory.NewCompositeGuard(memory.DefaultGuard{}, exact)`; no production write path silently substitutes a generic-only guard.

Use `//go:build darwin || linux` in `path_unix.go`; unsupported platforms return a safe unsupported-platform error from a small complementary file without weakening checks. Resolve the nearest existing ancestor to a canonical physical path, create each missing dedicated directory at `0700` without accepting a symlinked replacement, then open and retain the final dedicated-parent directory descriptor. Require an existing final parent to be current-user-owned, real, and have no group/world permission bits; never chmod a pre-existing directory implicitly. Use descriptor-relative `openat` with `O_NOFOLLOW|O_CLOEXEC` to validate or atomically create the final DB at `0600`; if concurrent create returns `EEXIST`, restart validation rather than truncating or trusting it. Retain the verified DB descriptor until Store close. Reject an existing non-regular, non-current-user-owned, or group/world-accessible DB, `-wal`, or `-shm`; pre-existing sidecars must also be no-follow-opened and inode/mode/owner checked. Immediately before both the read-only preflight physical connection and the later first write-capable physical connection, snapshot the process's open-FD inode multiset under a package initialization mutex. While the new `*sql.Conn` remains retained, enumerate `/dev/fd` on Darwin or `/proc/self/fd` on Linux with `fstat` (never trust symlink text), require a newly opened regular descriptor whose device/inode equals the retained DB descriptor, and require `PRAGMA database_list` to report the structured canonical filename. This is the connection-level proof: a hook that substitutes a different DB only during `sqlite3_open` and restores the pathname must fail despite post-open pathname equality. Also reopen the canonical parent path with no-follow semantics and compare its inode/device to the retained directory descriptor, then repeat descriptor-relative no-follow DB open and compare the pathname DB inode/device to the retained DB descriptor. Close preflight fully and repeat all proof steps before write open; abort on any mismatch before a persistent application PRAGMA/schema write. After WAL/SHM creation, snapshot and fstat SQLite's newly held sidecar descriptors where the VFS retains them, require them to match descriptor-relative validated sidecar inodes, and always repeat parent/DB identity and sidecar-entry checks. If the target VFS/platform cannot provide these FD proofs, fail closed with ErrUnsupported rather than falling back to pathname-only validation. Errors remain content/path-free. Construct `file:` URIs with `net/url`, never string-concatenate query content. First preflight an existing database through an explicit `mode=ro&_query_only=1` URI connection, apply the same sqlite3_limit ceilings before querying, then inspect `user_version` and `sqlite_schema`, and refuse unknown/corrupt layouts before any write-capable application PRAGMA or schema mutation; any SQLite read-side SHM is subject to the same no-follow/owner/mode validation. Only then open the normal read-write DSN, whose validated driver options are equivalent to:

```text
_defensive=1
_foreign_keys=on
_synchronous=FULL
_busy_timeout=5000
_txlock=immediate
_dqs=0
_pragma=trusted_schema(OFF)
```

After closing preflight and before borrowing/opening any write-capable connection, call the injected ID generator for bounded prospective database/user IDs and validate/guard them as opaque fields when an uninitialized schema may need them; schema initialization uses those values or discards them if another initializer won. Then open the write-capable DSN initially with one physical connection, apply/verify all per-connection limits and PRAGMAs on it, set persistent `PRAGMA journal_mode=WAL` under initialization serialization, and initialize/verify schema. Retain that first connection, acquire exactly three more (four total), apply/verify the same configuration on each, and retain all four in a Store-owned channel for the Store lifetime; all operations borrow/return one and never call DB-level Query/Exec. Set `MaxOpenConns(4)`, `MaxIdleConns(4)`, and unlimited lifetime, but retained connections prevent an unconfigured replacement from entering service. The steady-state DSN omits `_journal_mode`. On every retained connection call `modernc.org/sqlite.Limit` and verify immutable ceilings: LENGTH/SQL_LENGTH 131072 bytes, COLUMN 64, VARIABLE_NUMBER 1024, EXPR_DEPTH 100, COMPOUND_SELECT 50, FUNCTION_ARG 100, ATTACHED 0, TRIGGER_DEPTH 0, and WORKER_THREADS 0. Also use bounded SQL projections (`length(...)` gate before content columns) so corrupted rows map to `ErrCorrupt` without materializing content. After schema SQL releases its transaction/connection, validate and guard buffered existing identity values as opaque fields before publishing the Store. Close every retained connection and the DB on any setup failure/post-open cancellation. Driver open/DSN PRAGMAs are not assumed cancelable once started.

- [ ] **Step 5: Implement schema v1 in one immediate transaction**

Create exact tables/indexes:

```sql
CREATE TABLE memory_meta (
    key TEXT PRIMARY KEY CHECK (length(key) BETWEEN 1 AND 64),
    value TEXT NOT NULL CHECK (length(value) <= 256)
) STRICT;

CREATE TABLE memory_records (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    scope_namespace TEXT NOT NULL CHECK (length(scope_namespace) BETWEEN 1 AND 32),
    scope_id TEXT NOT NULL CHECK (length(scope_id) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (length(kind) <= 32),
    semantic_key TEXT NOT NULL CHECK (length(CAST(semantic_key AS BLOB)) <= 256),
    text_value TEXT NOT NULL CHECK (length(CAST(text_value AS BLOB)) <= 8192),
    labels_json TEXT NOT NULL CHECK (length(CAST(labels_json AS BLOB)) <= 8192 AND json_valid(labels_json) AND json_type(labels_json) = 'array'),
    metadata_json TEXT NOT NULL CHECK (length(CAST(metadata_json AS BLOB)) <= 4096 AND json_valid(metadata_json) AND json_type(metadata_json) = 'object'),
    source_json TEXT NOT NULL CHECK (length(CAST(source_json AS BLOB)) <= 8192 AND json_valid(source_json) AND json_type(source_json) = 'object'),
    confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30),
    updated_at TEXT NOT NULL CHECK (length(updated_at) = 30),
    expires_at TEXT CHECK (expires_at IS NULL OR length(expires_at) = 30),
    state TEXT NOT NULL CHECK (state IN ('active','tombstone')),
    forgotten_at TEXT CHECK (forgotten_at IS NULL OR length(forgotten_at) = 30),
    CHECK (updated_at >= created_at),
    CHECK (expires_at IS NULL OR expires_at >= created_at),
    CHECK (
        (state = 'active' AND forgotten_at IS NULL) OR
        (state = 'tombstone' AND forgotten_at IS NOT NULL AND forgotten_at = updated_at AND kind = '' AND semantic_key = ''
         AND text_value = '' AND labels_json = '[]' AND metadata_json = '{}'
         AND source_json = '{}' AND confidence = 0.0 AND expires_at IS NULL)
    )
) STRICT;

CREATE UNIQUE INDEX memory_records_key_active
ON memory_records(scope_namespace, scope_id, kind, semantic_key)
WHERE state = 'active' AND semantic_key <> '';

CREATE INDEX memory_records_list
ON memory_records(scope_namespace, scope_id, state, updated_at DESC, id ASC);

CREATE TABLE memory_observations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    candidate_ids_json TEXT NOT NULL CHECK (length(CAST(candidate_ids_json AS BLOB)) <= 1024 AND json_valid(candidate_ids_json) AND json_type(candidate_ids_json) = 'array'),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30)
) STRICT;

CREATE TABLE memory_candidates (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 64),
    scope_namespace TEXT NOT NULL CHECK (length(scope_namespace) BETWEEN 1 AND 32),
    scope_id TEXT NOT NULL CHECK (length(scope_id) BETWEEN 1 AND 128),
    action TEXT NOT NULL CHECK (action IN ('create','update','forget')),
    target_id TEXT NOT NULL CHECK (length(target_id) <= 64),
    base_revision INTEGER NOT NULL CHECK (base_revision >= 0),
    observation_id TEXT REFERENCES memory_observations(id) ON DELETE RESTRICT CHECK (observation_id IS NULL OR length(observation_id) BETWEEN 1 AND 64),
    proposed_json TEXT NOT NULL CHECK (length(CAST(proposed_json AS BLOB)) <= 32768 AND json_valid(proposed_json) AND json_type(proposed_json) = 'object'),
    reason TEXT NOT NULL CHECK (length(CAST(reason AS BLOB)) <= 2048),
    state TEXT NOT NULL CHECK (state IN ('pending','accepted','rejected')),
    created_at TEXT NOT NULL CHECK (length(created_at) = 30),
    decided_at TEXT CHECK (decided_at IS NULL OR length(decided_at) = 30),
    decision_source TEXT NOT NULL CHECK (length(decision_source) <= 32),
    result_record_id TEXT NOT NULL CHECK (length(result_record_id) <= 64),
    result_revision INTEGER NOT NULL CHECK (result_revision >= 0),
    CHECK (COALESCE(json_type(proposed_json, '$.scope_namespace'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.scope_namespace'), '') = scope_namespace),
    CHECK (COALESCE(json_type(proposed_json, '$.scope_id'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.scope_id'), '') = scope_id),
    CHECK (
        state <> 'pending' OR
        CASE WHEN observation_id IS NULL THEN
            COALESCE(json_extract(proposed_json, '$.source.observation_id'), '') = ''
        ELSE
            COALESCE(json_type(proposed_json, '$.source.observation_id'), '') = 'text'
            AND COALESCE(json_extract(proposed_json, '$.source.observation_id'), '') = observation_id
        END
    ),
    CHECK (
        (action = 'create' AND target_id = '' AND base_revision = 0) OR
        (action IN ('update','forget') AND target_id <> '' AND base_revision > 0)
    ),
    CHECK (
        (state = 'pending' AND decided_at IS NULL AND decision_source = ''
         AND result_record_id = '' AND result_revision = 0) OR
        (state = 'accepted' AND decided_at IS NOT NULL AND decided_at >= created_at AND decision_source <> '' AND reason = ''
         AND result_record_id <> '' AND result_revision > 0) OR
        (state = 'rejected' AND decided_at IS NOT NULL AND decided_at >= created_at AND decision_source <> '' AND reason = ''
         AND result_record_id = '' AND result_revision = 0)
    ),
    CHECK (
        state = 'pending' OR (
            COALESCE(json_type(proposed_json, '$.kind'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.kind'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.key'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.key'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.text'), '') = 'text' AND COALESCE(json_extract(proposed_json, '$.text'), '') = '' AND
            COALESCE(json_type(proposed_json, '$.labels'), '') = 'array' AND json_array_length(proposed_json, '$.labels') = 0 AND
            COALESCE(json_type(proposed_json, '$.metadata'), '') = 'object' AND json_extract(proposed_json, '$.metadata') = '{}' AND
            COALESCE(json_type(proposed_json, '$.source'), '') = 'object' AND json_extract(proposed_json, '$.source') = '{}' AND
            COALESCE(json_type(proposed_json, '$.confidence'), '') IN ('integer','real') AND json_extract(proposed_json, '$.confidence') = 0 AND
            COALESCE(json_type(proposed_json, '$.expiry'), '') = 'null'
        )
    )
) STRICT;

CREATE INDEX memory_candidates_list
ON memory_candidates(scope_namespace, scope_id, state, created_at DESC, id ASC);

CREATE INDEX memory_candidates_observation
ON memory_candidates(observation_id)
WHERE observation_id IS NOT NULL;

CREATE VIRTUAL TABLE memory_records_fts USING fts5(
    record_id UNINDEXED,
    text_value,
    kind,
    semantic_key,
    labels,
    tokenize = 'unicode61'
);
```

After acquiring the immediate transaction, re-read `user_version`: initialize only if it is still zero and `sqlite_schema` has no user objects; if another opener completed version one, verify and reuse it. Set `PRAGMA user_version=1` and insert `database_id`, `user_scope_id`, canonical decimal `generation=0`, and `schema_fingerprint`, a compiled SHA-256 over the canonical v1 migration/object manifest. Existing version zero with nonempty unknown objects is `ErrCorrupt`; never overwrite it. Version one must match the exact expected application object definitions and schema fingerprint, the expected FTS shadow-object names/columns (without pinning SQLite's internal shadow SQL text), and validated meta values. Added, missing, or changed objects fail closed as `ErrCorrupt`; do not auto-repair them in Open.

- [ ] **Step 6: Add write transaction and safe SQLite error helpers**

A Store admission mutex/condition (or equivalent wait group plus one close-owner channel) rejects new operations after closing begins and lets Close wait for every admitted read, write, and estimator callback before releasing retained connections/descriptors; concurrent Close calls share the same final result without double-close. A process-local capacity-one write gate serializes writes and is acquired with a context-aware select (never an uncancelable mutex wait). On the borrowed connection execute constant `BEGIN IMMEDIATE` directly rather than `sql.Conn.BeginTx`, so caller cancellation can interrupt reservation acquisition without giving `database/sql` a transaction-lifetime context that could race Commit. Bound total busy handling by Options.BusyTimeout: acquire an `*sql.Conn`, set that connection's integer `PRAGMA busy_timeout` to the remaining validated milliseconds before each attempt, and begin the immediate transaction on the same connection. SQLite's busy handler consumes that budget for ordinary cross-process contention, while only a pre-callback `BEGIN IMMEDIATE` primary-code `SQLITE_BUSY` returns the connection and may then retry with capped exponential jitter only while budget/context remains. `SQLITE_LOCKED` is never retried and returns `ErrBusy` immediately. An unexported injected retry-delay seam makes tests event-driven; production delay uses context-aware timers and no global RNG. Classify modernc extended result codes by their primary low byte plus explicit extended cases: map UNIQUE/PRIMARYKEY to `ErrConflict`, CHECK/NOTNULL/FK after prior validation to `ErrCorrupt`, busy/locked to `ErrBusy`, corrupt/not-a-database to `ErrCorrupt`, INTERRUPT to `ctx.Err()` only when the caller context is done (otherwise ErrUnavailable), and closed cases to `ErrClosed` using `errors.As` against the modernc SQLite error type; do not parse content-bearing SQL strings into user-facing errors. Run transaction statements with caller contexts and issue constant `ROLLBACK` with a non-canceling context on every pre-commit error; rollback failure discards the physical connection with `driver.ErrBadConn`, poisons the Store, and returns safe ErrUnavailable rather than recycling uncertain state. Construct and validate the operation's safe CommitUnknownError metadata before opening the transaction, so an unknown outcome never needs fallible error construction. Check caller context immediately before the commit point: cancellation rolls back. Execute constant `COMMIT` with an internal non-canceling context; once it starts, do not report later cancellation over success. Any driver COMMIT error is wrapped as safe `CommitUnknownError` because its outcome requires ID-based reconciliation; never return that possibly transactional connection to the channel. Discard its physical driver connection via `Conn.Raw` returning `driver.ErrBadConn` (not merely `sql.Conn.Close`), atomically close a poison signal so current connection/gate waiters and later operations return ErrUnavailable, and let Close release the remaining retained resources. Recovery is explicit close/reopen plus ID-based reconciliation.

- [ ] **Step 7: Run focused and repeated open tests**

```bash
go test ./internal/memory/sqlite -run 'TestOpen|TestSchema|TestIdentity' -count=20
go test -race ./internal/memory/sqlite -run 'TestOpen|TestIdentity' -count=10
```

Expected: PASS with no leaked `-wal`/`-shm` assertions after close beyond SQLite's documented empty cleanup behavior.

- [ ] **Step 8: Commit schema and dependency**

```bash
git add go.mod go.sum internal/memory/sqlite
git commit -m "feat: open SQLite memory store"
```

---

### Task 5: Persist, list, and reopen active records

**Files:**
- Create: `internal/memory/sqlite/codec.go`
- Create: `internal/memory/sqlite/cursor.go`
- Create: `internal/memory/sqlite/records.go`
- Create: `internal/memory/memorytest/store.go`
- Test: `internal/memory/sqlite/records_test.go`
- Test: `internal/memory/sqlite/conformance_test.go`

**Interfaces:**
- Consumes: Task 4 `Store`, schema, and transaction helper.
- Produces: `Identity`, `Get`, `GetByKey`, active `List`, deterministic cursor codec, create-only `Upsert`, reopen round-trip, and the reusable record-surface conformance harness.

- [ ] **Step 1: Write the first reusable conformance cases**

Define an exported test-helper-only `memorytest.RecordStore` interface containing `Identity`, `Get`, `GetByKey`, `List`, create-only `Upsert`, and `Close`. `memorytest.RunRecordConformance(t, factory)` accepts a factory returning a fresh `RecordStore` plus cleanup, so the adapter is not forced to add untested candidate stubs before Task 7. Subtests must assert:

- Create a fully populated Record at revision zero with nil expected revision returns revision 1, preserves every non-revision field exactly, and advances Identity.Generation from zero to one.
- Input labels/maps/source slices can be mutated after `Upsert` without changing stored state.
- Returned Record can be mutated without changing a later `Get`.
- Reopen preserves exact UTC timestamps, expiry, confidence, labels, metadata, and provenance.
- Empty key permits multiple records; duplicate non-empty key returns `ErrConflict` without changing either record, FTS, or Generation.
- After Open, an external test connection with `PRAGMA ignore_check_constraints=ON` inserts one-over-limit TEXT/JSON/redaction-marker corruption plus a 1 MiB value beyond the connection LENGTH ceiling; Get/List return only safe `ErrCorrupt`/`ErrSensitiveMemory`, never materialize the oversized value across the driver, and never echo it.
- `Get` and non-empty `GetByKey` return exact active records even when expired; wrong scopes/kinds/keys return `ErrNotFound`, while an empty semantic key returns `ErrInvalidRequest` without scanning all records.
- List filters by scopes, kinds, all requested labels, active state, and expiry with `ExpiresAt <= Now` treated as expired; `IncludeExpired=true` includes expired active records without including tombstones.
- Limit-two pagination over five equal-time records returns each ID exactly once in deterministic `updated_at DESC, id ASC` order; exact-second and fractional-nanosecond timestamps sort chronologically across the storage encoding boundary.
- Malformed, version-mismatched, cross-filter, oversized, and stale-generation cursors fail safely (`ErrInvalidCursor` for malformed/mismatched, `ErrConflict` for a valid cursor invalidated by any committed mutation) without echoing cursor content.
- Cancellation before commit leaves the database unchanged; a synthetic post-commit response loss returns `ErrCommitUnknown`, after which Get by the caller-generated ID proves whether create/update committed before any retry.
- A structurally valid record containing a redaction marker, credential shape, or exact synthetic forbidden value (including in an opaque ID/source field) returns `ErrSensitiveMemory`, persists no row/FTS entry, and does not echo content.

Call the harness from `sqlite/conformance_test.go` with close/reopen helpers.

- [ ] **Step 2: Run record/conformance tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestRecordConformance' -count=1
```

Expected: interface methods are missing or return the test stub failure.

- [ ] **Step 3: Implement deterministic codecs**

Use `encoding/json` for labels, metadata, provenance, and candidates; Go JSON sorting for map keys is required for deterministic bytes. Every SELECT uses SQL `length(CAST(column AS BLOB))` guards and CASE-to-NULL projections before any content-bearing column reaches the driver; an oversize marker maps to ErrCorrupt. Decode bounded bytes into fresh values and run neutral validation while SQL is open; buffer at most the page hard limit, close rows/transaction/connection, then run `GuardRecord`/`GuardCandidate` before returning any content-bearing value. Structural decode/validation failure is `ErrCorrupt`; sensitivity failure remains the safe `ErrSensitiveMemory`. Encode timestamps with the fixed-width UTC layout `2006-01-02T15:04:05.000000000Z` so SQLite lexical ordering is chronological; reject any noncanonical/invalid value on read with `ErrCorrupt`. Add test-only direct SQL corruption cases proving manually inserted redaction-marked or oversized TEXT/JSON content cannot cross Get/List projections or cause process-scale allocation; Task 8 applies the same assertion to Retriever.

Cursor payloads are versioned JSON encoded with raw URL-safe base64 and contain query fingerprint, Store generation, last timestamp, and last opaque ID. Cap raw base64 length before allocation, then cap decoded cursor bytes before JSON decode. The fingerprint is SHA-256 over canonical filter fields, including `IncludeExpired` and request `Now` when expiry filtering is active but excluding limit, so a cursor cannot cross scopes/kinds/labels/expiry mode.

- [ ] **Step 4: Implement create-only Upsert and FTS insertion**

Before beginning a transaction, clone the request, run `ValidateUpsertRequest`, and call `GuardRecord(ctx, s.guard, desiredRecord)`. For `ExpectedRevision == nil`, require incoming revision zero, insert a revision-one active row, insert the same record into FTS using a sorted-copy newline join for the labels column, and increment `generation`, all in one transaction. `Identity` reads and validates the current canonical unsigned-decimal generation rather than caching it; `bumpGeneration` detects `uint64` overflow and rolls the content transaction back with a safe `ErrCorrupt`. Only UNIQUE/PRIMARYKEY identity races map to safe `ConflictError`; CHECK/NOTNULL/FK failures after prior validation map to `ErrCorrupt`, matching the shared SQLite error helper. Record IDs are already generated by the caller; the Store never uses row IDs.

- [ ] **Step 5: Implement Get and List**

For List, begin a read-only transaction, read generation first to establish the snapshot, compare a cursor generation before querying rows, and return `ErrConflict` on mismatch so concurrent writes cannot cause duplicate/skipped pagination; encode that snapshot generation in NextCursor. `Get` selects by ID plus scope and active state; `GetByKey` uses the unique `(scope, kind, semantic_key)` index and returns `ErrInvalidRequest` for an empty key. Exact management reads may return an expired active record. `List` constructs parameterized `IN`/`EXISTS json_each` clauses, applies expiry using request `Now` unless `IncludeExpired` is true, orders deterministically, fetches `limit+1`, and returns an opaque cursor only when another row exists. Empty scopes mean no results, not all scopes. Hard-limit every page even if a malformed adapter caller bypasses Manager validation.

- [ ] **Step 6: Run conformance, reopen, and alias stress**

```bash
go test ./internal/memory/sqlite -run 'TestRecordConformance' -count=50
go test -race ./internal/memory/sqlite -run 'TestRecordConformance' -count=10
```

Expected: PASS.

- [ ] **Step 7: Commit active record persistence**

```bash
git add internal/memory/memorytest internal/memory/sqlite/codec.go internal/memory/sqlite/cursor.go internal/memory/sqlite/records.go internal/memory/sqlite/records_test.go internal/memory/sqlite/conformance_test.go
git commit -m "feat: persist SQLite memory records"
```

---

### Task 6: Add conditional updates and tombstones

**Files:**
- Modify: `internal/memory/memorytest/store.go`
- Modify: `internal/memory/sqlite/records.go`
- Modify: `internal/memory/sqlite/records_test.go`
- Test: `internal/memory/sqlite/mutation_test.go`

**Interfaces:**
- Consumes: Task 5 record codec/cursors/transactions.
- Produces: revision-checked update Upsert, Forget, GetTombstone, ListTombstones, exact FTS removal/update, conflict safety, and `memorytest.MutationStore`/`RunMutationConformance` extending the Task 5 surface.

- [ ] **Step 1: Extend conformance with update/forget cases**

Add `memorytest.MutationStore`, embedding `RecordStore` and adding `GetTombstone`, `ListTombstones`, and `Forget`, then run these literal cases through `RunMutationConformance`:

- Update input revision 1 with expected 1 returns revision 2, rejects a changed CreatedAt, uses a later supplied UpdatedAt/content, and replaces exactly one FTS row.
- Update with expected zero returns `ErrInvalidRequest`; old/too-new positive revisions return `ErrConflict`; all leave bytes/revision/FTS/Generation unchanged.
- Update cannot change ID or scope; semantic-key conflicts are atomic.
- Forget with exact revision and ForgottenAt not before current UpdatedAt creates a content-free Tombstone at revision+1, clears text/labels/metadata/source/confidence/expiry from live storage, removes FTS, and makes Get/List return not found; an earlier ForgottenAt is rejected without mutation.
- Forget with stale revision is a no-op conflict.
- Repeated Forget with the resulting tombstone revision is idempotent and returns the existing Tombstone; unrelated positive revisions conflict and zero is invalid. On `ErrCommitUnknown`, GetTombstone by ID/scope is the required reconciliation before retry.
- `GetTombstone` returns an exact content-free tombstone by scope/ID and ErrNotFound for active/wrong-scope IDs; `ListTombstones` paginates deterministically by `ForgottenAt DESC, ID ASC`, is generation-bound, and returns no content fields.
- Reopen preserves tombstones; stale Upsert/Review against the tombstoned record ID cannot reactivate it. A model/extractor same-key re-remember can exist only as a new pending Candidate until explicit review (a separate explicit human Remember remains directly authorized), proving deliberate re-remembering is distinct from stale resurrection.

- [ ] **Step 2: Run focused tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestMutationConformance|TestMutation' -count=1
```

Expected: current create-only behavior fails exact revision/tombstone assertions.

- [ ] **Step 3: Implement conditional update**

Guard the desired Record and bounded existing snapshot after releasing the read connection, then require `Record.Revision == *ExpectedRevision`, preserve and verify CreatedAt, and require UpdatedAt not to move backwards. Inside the write transaction, compare the row's canonical digest/revision to that guarded snapshot before mutation. Use one `UPDATE ... WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?`. Check `RowsAffected()==1`; otherwise read only ID/revision/state: a matching Tombstone is always ConflictError (never reactivated), an absent ID/scope is ErrNotFound, and any other revision/state mismatch is ConflictError. Update FTS by deleting the old record ID then inserting the new text in the same transaction. Increment generation exactly once.

- [ ] **Step 4: Implement Forget and tombstone listing**

Forget first bounded-reads/releases/guards the current Record or Tombstone, then inside the write transaction verifies its canonical digest/revision and requires ForgottenAt not before current UpdatedAt before updating state, revision, updated/forgotten time, and clears every content-bearing column, including kind, semantic key, text, labels, metadata, provenance, confidence, and expiry. Preserve only ID, scope, original CreatedAt, revision, UpdatedAt, and ForgottenAt required by Tombstone. Delete FTS in the same transaction. Idempotent repeated Forget must not increment generation again. `ListTombstones` uses the same generation-bound read transaction/keyset cursor contract as active List.

- [ ] **Step 5: Run high-count and race tests**

```bash
go test ./internal/memory/sqlite -run 'Test(Record|Mutation)Conformance|TestMutation' -count=50
go test -race ./internal/memory/sqlite -run 'Test(Record|Mutation)Conformance|TestMutation' -count=10
```

- [ ] **Step 6: Commit revision and forget semantics**

```bash
git add internal/memory/memorytest/store.go internal/memory/sqlite/records.go internal/memory/sqlite/records_test.go internal/memory/sqlite/mutation_test.go
git commit -m "feat: enforce memory record revisions"
```

---

### Task 7: Persist candidates and idempotent observations

**Files:**
- Create: `internal/memory/sqlite/candidates.go`
- Test: `internal/memory/sqlite/candidates_test.go`
- Modify: `internal/memory/memorytest/store.go`
- Modify: `internal/memory/sqlite/conformance_test.go`

**Interfaces:**
- Consumes: Store transactions, codecs, Upsert/Forget semantics.
- Produces: Propose, GetObservationReceipt, CommitObservation, GetCandidate, ListCandidates, transactional Review for create/update/forget/reject, compile-time satisfaction of the complete neutral Store, and `RunCandidateConformance`.

- [ ] **Step 1: Add candidate conformance cases first**

Cover:

- `Propose` persists a batch atomically, preserves order in the returned batch, deep-clones values, and increments generation once.
- Duplicate candidate ID, any invalid member, or any proposal rejected by the injected composite Guard rejects the entire batch without echo or persistence.
- `GetObservationReceipt` is a bounded content-free exact probe used by the future Binding before extraction; unknown ID returns ErrNotFound. `CommitObservation` commits the receipt and zero-to-eight candidates together and increments Generation exactly once even for an empty first receipt; an already-committed same ID returns the original candidate IDs with `Existing=true` before validating a different retry payload and never increments Generation again.
- A failed candidate insert leaves no observation receipt; source ObservationID, candidate FK, and receipt candidate-ID order correspond exactly, and direct orphan/mismatched corruption returns `ErrCorrupt` without content.
- GetCandidate requires scope+ID, returns fresh content, and wrong scope is ErrNotFound; List filters by state/scope and paginates `created_at DESC, id ASC` with the generation-bound cursor contract, without leaking proposal text into cursors.
- Reject changes pending to rejected, clears proposed text/labels/metadata and reason immediately, records decision source/time, and creates no record.
- Accept create/update with nil Edited uses Candidate.Proposed; supplied Edited content must satisfy proposed-content validation, retain Candidate Scope, and pass the guard.
- Accept create requires a caller-generated ResultRecordID, atomically inserts that revision-one Record with durable decision provenance, and resolves Candidate; missing/extra/colliding result IDs leave Candidate pending.
- With two Store handles, an intervening update changes metadata at revision 2; rebase with nil Edited is rejected, while Edited content built from refreshed revision 2 preserves that metadata and applies the intended change at revision 3.
- Accept update/forget defaults to Candidate.BaseRevision. A stale conflict leaves Candidate pending; update retry with an explicitly refreshed current TargetRevision requires non-nil Edited full content and succeeds only if that revision still matches atomically, while forget rebase needs no Edited content. Edited acceptance is fully revalidated and guarded, and a sensitive edit leaves Candidate/target unchanged.
- Accept forget uses the target base revision and creates a Tombstone atomically.
- Two reviews race: exactly one succeeds; the other returns `ErrConflict`.
- If accepted record mutation conflicts, Candidate remains pending.
- Reopen preserves receipts, decisions, and cleared payload state. On `ErrCommitUnknown`, callers reconcile Propose/Review through ListCandidates plus ResultRecordID/target reads; observation retries use their durable receipt directly.

- [ ] **Step 2: Run candidate tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestCandidateConformance|TestCandidate' -count=1
```

Expected: missing methods or stub failures.

- [ ] **Step 3: Implement proposal and observation transactions**

Propose clones, validates, and guards its whole bounded batch before borrowing a write connection, then inserts all Candidates and bumps generation once in one immediate transaction. Encode each Proposed Record with a private canonical lower-snake-case JSON object whose top level includes `scope_namespace`, `scope_id`, kind/key/text/labels/metadata/source/confidence/expiry; do not marshal the public Go struct shape accidentally. Expose a bounded `GetObservationReceipt` SELECT that buffers capped candidate IDs, releases all SQL resources, then calls `GuardObservationReceipt`; the future Binding uses it to short-circuit duplicate Observe before Extractor. `CommitObservation` first does the same receipt probe; a pre-existing receipt returns before alternate payload validation. On a miss, enforce batch/count/aggregate caps and clone, validate, `GuardObservationCommit`, and `GuardCandidate` every proposal before borrowing a write connection. The immediate transaction rechecks the receipt to close concurrent races. If one now exists, bounded-buffer it, roll back/release the no-op transaction, then guard and return it. Otherwise insert the receipt first and each candidate with its FK `observation_id`; require the new receipt's ordered candidate-ID array, inserted candidate rows, and Proposed.Source.ObservationID to correspond exactly, then bump Generation once. A concurrently racing invalid alternate may fail before the winner commits, but its later retry deterministically returns the receipt; no duplicate can commit. Cap IDs/counts before stored JSON decode and treat invalid stored JSON as `ErrCorrupt`.

- [ ] **Step 4: Implement candidate listing and Review**

Before borrowing a write connection, bounded-read/release/guard a pending Candidate snapshot and, for update/forget, its exact target snapshot; wrong Candidate scope is ErrNotFound and a target found only as a Tombstone is ErrConflict. Clone/validate Edited, resolve/restrict any rebase, materialize the prospective accepted Record/Tombstone, and guard every request/snapshot/materialized persisted field outside SQL. Reject and accept-forget require nil Edited; accepted create/update require its Scope to equal the Candidate Scope. For accepted create, require the caller-supplied ResultRecordID, set revision one, set CreatedAt/UpdatedAt and Source.DecisionAt/DecisionSource to the review decision, and validate/guard the fully materialized Record including expiry. For accepted update, require DecidedAt not before target UpdatedAt, preserve target ID/CreatedAt, set UpdatedAt and Source.DecisionAt/DecisionSource to the review decision, increment revision, and validate/guard it. For accepted forget, require DecidedAt not before target UpdatedAt and use it as UpdatedAt/ForgottenAt.

The immediate transaction reloads bounded Candidate/target bytes, performs internal decode/validation only, and compares canonical digests plus revision/state to the guarded snapshots; any change conflicts, so no unguarded value can enter a mutation. Use transaction-local helpers that never bump generation themselves. Only after the accepted mutation succeeds update candidate state/result fields, replace `proposed_json` with a canonical payload retaining only Proposed.Scope while clearing Proposed ID/kind/key/text/labels/metadata/source/confidence/expiry/timestamps, and clear reason. Reject performs only the content-cleared decision update. Increment generation once per successful review; release SQL before returning deep clones.

- [ ] **Step 5: Exercise transactional fault seams**

Inject adapter-local test hooks immediately before candidate decision update, immediately before transaction commit, and immediately after successful driver commit but before response publication. Pre-commit errors leave record/candidate/observation state unchanged after reopen; the post-commit hook returns `ErrCommitUnknown` while reconciliation proves the mutation committed exactly once. Hooks exist only in unexported test seams and cannot bypass validation in production.

- [ ] **Step 6: Run stress and race tests**

```bash
go test ./internal/memory/sqlite -run 'Test(Record|Mutation|Candidate)Conformance|TestCandidate' -count=50
go test -race ./internal/memory/sqlite -run 'Test(Record|Mutation|Candidate)Conformance|TestCandidate' -count=10
```

- [ ] **Step 7: Commit candidate durability**

```bash
git add internal/memory/memorytest/store.go internal/memory/sqlite/candidates.go internal/memory/sqlite/candidates_test.go internal/memory/sqlite/conformance_test.go
git commit -m "feat: persist memory candidates"
```

---

### Task 8: Implement bounded SQLite FTS retrieval

**Files:**
- Create: `internal/memory/sqlite/query.go`
- Create: `internal/memory/sqlite/retriever.go`
- Test: `internal/memory/sqlite/query_test.go`
- Test: `internal/memory/sqlite/retriever_test.go`

**Interfaces:**
- Consumes: active Store rows/FTS maintained by Tasks 5–7 and neutral `Retriever` types.
- Produces: a deterministic lexical Retriever with user baseline, workspace precedence, dedupe, expiry, count, and token budgets.

- [ ] **Step 1: Write safe-query tests**

Assert raw input is converted to quoted FTS terms rather than accepted as FTS syntax. Include empty/whitespace, punctuation-only, quotes, boolean operators, wildcard, column syntax, SQL-looking input, UTF-8, and one-over-limit query/term-count/term-size cases. Tests compare exact generated term expressions for ASCII literals and verify no returned error contains the query.

- [ ] **Step 2: Write retrieval behavior tests**

Create real records and assert:

- Empty query with IncludeBaseline=true returns only the newest bounded keyed user `preference`/`instruction` baseline records plus required keyed workspace replacements; keyless records require an FTS match and are never injected on every turn. IncludeBaseline=false returns no matches for an empty Retriever query because complete management listing uses Store.List.
- Relevant FTS records from allowed user/workspace scopes appear; other scopes never appear.
- A workspace keyed record suppresses and replaces a same `(Kind, Key)` user record even when only the user text matches the query; retrieval permits at most one user and one workspace Scope in Phase 1 so precedence is unambiguous.
- Identical normalized content is returned once, preferring workspace then newer update then lexical rank then ID.
- Tombstoned records never appear; expired (`ExpiresAt <= Now`) records appear only when IncludeExpired=true.
- Kind and all-label filters apply to baseline, FTS, and workspace-replacement paths.
- Limit-two pagination over five eligible records returns each once across opaque generation-bound pages; ranks continue `1..5`, cross-query/filter cursors return `ErrInvalidCursor`, and a committed mutation between pages returns `ErrConflict`.
- `Limit=3` returns exactly three in deterministic order across 100 runs, with final RetrievalMatch ranks `1,2,3` independent of raw BM25 float values.
- Token estimator receives a deterministic budget string containing record ID, scope, kind, key, sorted labels, and text; a nonpositive estimate is conservatively treated as one token, overflow saturates, UsedTokens equals the included whole-record sum, a record individually larger than the whole page budget is skipped with cursor progress, while a record that fits an empty page but not the remaining budget starts the next page; no partial text is returned and pagination cannot loop on an oversized record.
- Query containing `" OR 1=1 --` cannot broaden results or break SQL.
- Returned Records do not alias Store state.

Use an injected literal estimator in tests:

```go
func words(text string) int { return len(strings.Fields(text)) }
```

- [ ] **Step 3: Run retrieval tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestBuildFTS|TestRetriever' -count=1
```

Expected: missing query/retriever implementation.

- [ ] **Step 4: Implement term construction and candidate fetch**

Validate and cap query bytes before tokenization. Build at most 64 nonempty Unicode term runs of at most 256 UTF-8 bytes each; reject rather than silently truncate a one-over-limit input. Escape embedded quotes by doubling them, quote each term, and join with `OR`. Never concatenate scope/kind values; generate placeholders and bind every value.

Use one read-only transaction for generation, baseline, FTS, and workspace-replacement queries so retrieval sees one coherent WAL snapshot. Reuse the versioned safe cursor codec with a retrieval fingerprint over query/scopes/kinds/labels/time/include flags, snapshot generation, global last-examined ordinal and opaque record ID; cursor JSON contains only hashes, enums, times, ranks, and opaque IDs—not query/content. A valid stale generation returns `ErrConflict`. When IncludeBaseline is true, fetch at most 16 active keyed user `preference`/`instruction` baseline rows plus a bounded oversample of BM25 FTS rows. For keyed user candidates, issue one bounded parameterized lookup—not one query per key—for active same-key workspace replacements that also satisfy the IncludeExpired policy even when their text did not match FTS. Any such workspace record suppresses the user value; add the replacement only if it also satisfies requested labels, so filtering cannot resurrect an overridden user value. FTS candidate SQL joins `memory_records_fts.record_id` to active records and filters scopes/kinds and conditionally expiry according to IncludeExpired before ordering by `bm25(memory_records_fts, 0.0, 1.0, 0.5, 2.0, 1.0)` (record ID unweighted; text/kind/key/labels fixed weights), updated time, and ID. The oversample has an immutable ceiling so dedupe cannot cause an unbounded query. Map SQLite/FTS failures to safe categories without wrapping the generated expression or original query text.

- [ ] **Step 5: Implement merge, precedence, and budget**

Normalize content fingerprints with trimmed Unicode whitespace and SHA-256; do not lowercase or alter stored text. Resolve keyed workspace-over-user precedence before content dedupe. Sort with explicit stable comparison: baseline class first, workspace before user, lower BM25 lexical rank first, updated descending, ID ascending. Commit/rollback the read transaction and close all SQL rows/connections before invoking ContentGuard on each bounded decoded candidate and then the caller-supplied estimator; never hold Store locks across it. Check context between candidates, skip everything at/before a valid cursor tuple, apply hard result count and token budget last, track both last returned and last examined tuples so individually over-budget rows cannot loop, and emit NextCursor only when another eligible whole record exists within the immutable 256-candidate ceiling. Estimate each whole record from a deterministic string containing ID, scope, kind, key, sorted labels, and text. When no estimator is supplied, use the same conservative byte estimate as `internal/agent`: zero for empty text, otherwise `1 + (len([]byte(text))-1)/3`; duplicate the tiny formula to avoid an agent↔memory package cycle.

- [ ] **Step 6: Run deterministic stress/race tests**

```bash
go test ./internal/memory/sqlite -run 'TestBuildFTS|TestRetriever' -count=100
go test -race ./internal/memory/sqlite -run 'TestRetriever' -count=20
```

- [ ] **Step 7: Commit FTS retrieval**

```bash
git add internal/memory/sqlite/query.go internal/memory/sqlite/query_test.go internal/memory/sqlite/retriever.go internal/memory/sqlite/retriever_test.go
git commit -m "feat: retrieve scoped SQLite memory"
```

---

### Task 9: Publish the SQLite Factory and harden Phase 1

**Files:**
- Create: `internal/memory/sqlite/factory.go`
- Test: `internal/memory/sqlite/factory_test.go`
- Test: `internal/memory/sqlite/concurrency_test.go`
- Test: `internal/memory/sqlite/process_test.go`
- Modify: `internal/memory/memorytest/store.go`
- Modify: `internal/memory/sqlite/conformance_test.go`
- Modify: `docs/superpowers/specs/2026-08-29-extensible-memory-design.md` only if implementation proves a factual driver/schema constraint differs; do not add README claims.

**Interfaces:**
- Consumes: all Phase 1 contracts and adapter behavior.
- Produces: `sqlite.Factory`, complete `memory.Components`, optional-feature capabilities, reusable conformance coverage, and a gate-clean Phase 1 branch.

- [ ] **Step 1: Write Factory/capability tests**

Assert `Factory.Open` returns non-nil Store, Retriever, and Maintenance; Identity is accessible; all five Maintenance methods, including PurgeForgotten, return `ErrUnsupported`; and exact capabilities are:

```go
Capabilities{
    LexicalSearch:      true,
    SemanticSearch:     false,
    OnlineBackup:       false,
    EncryptionAtRest:   false,
    ConcurrentProcesses: false,
}
```

Closing the Store makes adapter operations return `ErrClosed`; unsupported Maintenance remains content-free and does not panic. Task 9 hardens both direct Open and Factory.Open so reopen first requires bounded `PRAGMA quick_check` rows to equal `ok`, then runs SQL-level bounded-shape/domain scans plus FTS5 integrity/parity checks: missing, duplicate, orphaned, or stale FTS rows return `ErrCorrupt`; every still-present observed Candidate must reference a receipt containing its ID, and each pending observed Candidate's decoded Source.ObservationID must match that receipt (receipt IDs may outlive Candidate retention), and violations fail without auto-repair or content echo.

- [ ] **Step 2: Write two-handle and subprocess concurrency tests**

Open the same database through two Factory instances and assert:

- Both identities match.
- Concurrent create of the same keyed identity yields one success and one `ErrConflict`.
- Concurrent update at revision 1 yields one revision-2 success and one conflict; no lost update.
- Concurrent observation with the same ID yields one new and one existing receipt with identical candidate IDs.
- Concurrent review yields one decision and one conflict.
- Readers continue to return complete old-or-new records during writes; no partially decoded data appears.
- Closing one handle does not close the other; after every ordinary failure/stress case each Store reports all four retained connections returned to its channel before Close; the injected driver-COMMIT poison case reports exactly one closed/quarantined plus three returned, with all four accounted for.
- Event-gated concurrent Close waits for operations blocked at connection acquisition, SQL execution, pre-commit, commit-response, and blocking ContentGuard/token-estimator callbacks; test hooks assert every retained connection is back in the channel and no SQL transaction or lifecycle mutex is held while each callback blocks. Operations admitted first finish truthfully, later calls return ErrClosed, and ten concurrent Close callers receive one idempotent safe result with no race/deadlock.

Use start barriers, unique opaque IDs/keys per repetition, and 100 repetitions; never use arbitrary sleeps.

Add an offline `os/exec` test-helper mode in `process_test.go`. Parent-created stdin/stdout pipes provide ready/release/result barriers and every child has a hard context deadline. As pre-capability groundwork only (Factory remains ConcurrentProcesses=false until Phase 2 adds the shared lifetime lock and local-filesystem validation), across independently opened child processes assert: two simultaneous virgin-database initializers converge on one identity/schema with no half-bootstrap, four unique creates all survive, same semantic-key create/update/review races each have exactly one success and one `ErrConflict`, duplicate observation IDs converge on the first committed candidate-ID receipt, a child cannot revive a tombstoned record ID with a stale update, and a child intentionally exiting after commit but before response is reconciled by caller-generated ID without duplicate mutation. Child output contains only fixed status categories and opaque IDs, never content. Skip only on platforms the repository does not target; default macOS/Linux tests need no SQLite CLI.

- [ ] **Step 3: Run tests and observe RED**

```bash
go test ./internal/memory/sqlite -run 'TestFactory|TestConcurrent|TestProcess' -count=1
```

Expected: missing Factory/unsupported Maintenance or failed exact capabilities.

- [ ] **Step 4: Implement Factory and unsupported Maintenance**

Expose `NewFactory(path string, options Options) Factory`; empty path is `ErrInvalidRequest` in this phase because default-path resolution belongs to config/runtime. Factory stores a copied path and validated adapter Options. `Open` calls SQLite Open once and returns the Store plus a Retriever sharing that Store's pool without owning a second close. Maintenance is a non-nil adapter whose five methods check context then return `ErrUnsupported`. Do not register a global runtime backend map in this phase; `cmd/otto` registration belongs to Phase 3.

- [ ] **Step 5: Run package and conformance stress**

```bash
go test ./internal/memory/... -count=1
go test ./internal/memory/sqlite -run 'Test(Record|Mutation|Candidate)Conformance|Test(Concurrent|Retriever|Factory)' -count=50
go test ./internal/memory/sqlite -run 'TestProcess' -count=10
go test -race ./internal/memory/... -count=1
```

Expected: PASS.

- [ ] **Step 6: Run full repository gates**

```bash
go test -count=1 ./...
go test -race -count=1 ./...
CGO_ENABLED=0 go test -count=1 ./internal/memory/...
go test ./cmd/otto -run 'Test(TUIPseudoTerminalResumeLifecycle|TUICompactCommandCompletionCancelAndTerminalRestore|TUIPseudoTerminalLifecycle)$' -count=10
go vet ./...
GOPROXY="file://$(go env GOMODCACHE)/cache/download" GOSUMDB=off \
  go run honnef.co/go/tools/cmd/staticcheck@v0.8.1 ./...
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT; go build -trimpath -o "$tmp/otto" ./cmd/otto
test -z "$(gofmt -l .)"
git diff --check
```

Before this final sequence, separately run the repository-mandated `go run honnef.co/go/tools/cmd/staticcheck@latest -version`; it must resolve to the locally license-checked `v0.8.1`/`staticcheck 2026.2.1`, otherwise update and re-review the explicit pin before proceeding. The authoritative final lint invocation above is exact-version and forced through the local module-cache proxy with checksum-network lookup disabled, so the recorded gate is offline-reproducible. Default tests are offline. The build artifact must be outside the worktree. Confirm `git status --short --ignored` contains no generated database, WAL, SHM, test binary, or root `otto` artifact.

- [ ] **Step 7: Perform Phase 1 requirement review**

Read the Phase 1 diff against the spec and explicitly verify:

- Agent/session/config/tool/frontend files are untouched except dependency files.
- Store interfaces have no SQLite type.
- SQLite exact round-trip, revision, tombstone, candidate, observation, FTS, pagination, two-handle, and baseline subprocess semantics pass conformance; the capability remains false and no production support is claimed before Phase 2 lifetime locking/filesystem checks.
- No API result aliases mutable Store state.
- No errors contain record/query/proposal content.
- No backup, restore, lock-file, extractor, vector, remote, or provider behavior was added.

Correct any finding with a new failing regression test before changing production code.

- [ ] **Step 8: Commit final Phase 1 hardening**

```bash
git add internal/memory internal/memory/sqlite go.mod go.sum docs/superpowers/specs/2026-08-29-extensible-memory-design.md
git commit -m "feat: complete SQLite memory core"
```

If the spec was unchanged, omit it from `git add`. If no post-review correction is needed, the final commit contains only Factory/concurrency files rather than an empty commit.

---

## Phase 1 completion boundary

At the end of this plan, production code can instantiate and exercise the neutral SQLite Memory components in tests or future wiring, but normal Otto startup does not open a memory database, alter provider requests, register memory tools, parse memory config, or expose UI/CLI commands. That behavior belongs to later plans and PRs.

A separate implementation plan is required for each remaining design phase:

1. Durability locks, migrations, forget ledger, backup/restore, and maintenance CLI.
2. Runtime config, explicit management, tool result overlays, request-local Recall, TUI, and REPL.
3. Runtime-scoped LLM extraction, Observe, policy configuration, and candidate review events.
