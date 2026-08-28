# Pi v3 Sessions and Interactive Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Otto's private linear session persistence with Pi-compatible v3 tree JSONL and add a transactional, responsive TUI `/resume` picker for the current workspace's 20 most recent sessions.

**Architecture:** `internal/session` owns Pi v3 wire types, append-only tree persistence, context construction, and listing. `internal/app.Controller` owns transactional session/runtime replacement through injected provider-neutral factories. `cmd/otto` resolves concrete config/provider/tool runtimes, while `internal/tui` asynchronously lists and switches sessions through typed Bubble Tea messages.

**Tech Stack:** Go, standard-library JSON/filesystem/concurrency packages, Bubble Tea v2, Bubbles v2, Lip Gloss v2, existing Otto provider-neutral model/agent/session contracts, Pi v3 JSONL fixtures.

**Spec:** `docs/superpowers/specs/2026-08-27-pi-v3-sessions-resume-design.md`

## Global Constraints

- Compatibility target is `@earendil-works/pi-coding-agent` 0.84.3 session format version 3.
- New persistent sessions use Pi v3 only; old Otto v1 files remain untouched and unsupported.
- Stage 1 provider support remains `openai-compatible` only.
- Default tests stay offline and require neither Pi/Node, network, credentials, nor a real terminal.
- Persistent writes remain append-only after creation and call `Sync()` before success.
- Session JSONL limits are 16 MiB per entry and 256 MiB per file.
- Credentials, OAuth tokens, authorization headers, and resolved secret values never enter session files, logs, fixtures, or errors.
- Session listing is read-only, rejects symlinks/non-regular files, and requires canonical workspace equality.
- TUI presentation state mutates only in Bubble Tea `Update`; workers communicate through bounded typed messages.
- Resume must be transactional: every failure before swap leaves the current session/runtime usable.
- `bash` remains unsandboxed and starts in the selected workspace.
- TDD is mandatory: write and observe a focused failing test before production changes.
- Preserve package boundaries in `AGENTS.md`; provider wire types stay in `internal/provider/openaicompat`, session wire types stay in `internal/session`.

## Execution Profiles

Use the least expensive model/effort that can safely complete each task:

- **Schema, tree/context, and controller concurrency:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.
- **Filesystem listing, CLI runtime wiring, and TUI state machine:** `openai-codex/gpt-5.4`, `high` thinking; escalate to `gpt-5.6-sol xhigh` if concurrency or lifecycle behavior is unclear.
- **Responsive rendering, fixtures, docs, and mechanical integration:** `openai-codex/gpt-5.4-mini`, `medium` thinking; use `gpt-5.4 high` for failing PTY/race tests.
- **Task reviewers:** `openai-codex/gpt-5.4`, `high` thinking.
- **Final whole-branch reviewer and residual lifecycle/security fixes:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.

---

## File Structure

### Session core

- Create `internal/session/pi_types.go` — Pi v3 file/header/entry/message wire structs and raw-entry envelope.
- Create `internal/session/pi_codec.go` — bounded LF-delimited decoding, strict supported-entry decoding, raw unknown-entry preservation, and encoding.
- Create `internal/session/pi_codec_test.go` — schema, fixture, raw preservation, and size-bound tests.
- Create `internal/session/context.go` — tree indexing, active-path traversal, runtime metadata, Pi-message conversion, compaction/context construction, and usage reconstruction.
- Create `internal/session/context_test.go` — active branch, compaction, custom context, model/runtime, and unsupported-content tests.
- Create `internal/session/list.go` — read-only current-workspace session inspection/listing.
- Create `internal/session/list_test.go` — ordering, limit, skipped count, symlink, workspace, current marker, and no-mutation tests.
- Create `internal/session/testdata/pi-v3/*.jsonl` — documented Pi v3 conformance fixtures.
- Modify `internal/session/types.go` — public errors, v3 header/runtime/list types, warnings, and `Session` contract.
- Modify `internal/session/store.go` — Pi v3 create/open/append/sync/recovery/dangling-tool repair.
- Modify `internal/session/store_test.go` — replace private-v1 assertions with Pi v3 persistence and reliability assertions.
- Modify `internal/session/memory.go` — preserve in-memory behavior under the updated public contract.
- Delete `internal/session/persisted.go` after all old private wire types are unused.

### Provider-neutral context

- Modify `internal/model/types.go` — context-message role and metadata required for compaction/branch/custom context.
- Modify `internal/model/types_test.go` — defensive/context metadata behavior.
- Modify `internal/provider/openaicompat/protocol.go` — map provider-neutral context messages to safe OpenAI user context.
- Modify `internal/provider/openaicompat/client_test.go` — context translation tests.
- Modify `internal/tui/entries.go` and `internal/tui/entries_test.go` — render or hide resolved context messages deterministically.

### Application lifecycle

- Modify `internal/app/controller.go` — optional session browser contract, replacement bundle, resume result, shared replacement protocol, and transactional switch.
- Modify `internal/app/controller_test.go` — resume/list/cancel/close/race/failure lifecycle tests.

### CLI construction

- Create `cmd/otto/runtime_builder.go` — reusable runtime resolution, registry/provider/runner construction, session replacement factory, and warning transport.
- Create `cmd/otto/runtime_builder_test.go` — stored-runtime precedence, external Pi fallback, unsupported provider, missing profile/key, and cleanup tests.
- Modify `cmd/otto/main.go` — v3 creation/open/continue, builder injection, and controller resume wiring.
- Modify `cmd/otto/main_test.go` — Pi fixtures and CLI resume/continue/no-session behavior.

### TUI picker

- Create `internal/tui/resume.go` — picker state, generation-tagged list/resume commands, selection/page helpers, and relative-time formatting.
- Create `internal/tui/resume_test.go` — loading/results/stale/error/success/current/no-session tests.
- Modify `internal/tui/commands.go` — `/resume` registry entry.
- Modify `internal/tui/messages.go` — list/resume typed result messages.
- Modify `internal/tui/model.go` — command dispatch, modal key routing, async lifecycle, and successful transcript replacement.
- Modify `internal/tui/layout.go` — responsive picker rendering and clipping.
- Modify `internal/tui/keymap.go` — picker bindings using exact unmodified keys.
- Modify `internal/tui/responsive_test.go` and `internal/tui/security_test.go` — minimum-size and untrusted-metadata coverage.

### Integration and docs

- Modify `cmd/otto/tui_pty_test.go` — offline `/resume` lifecycle smoke coverage.
- Create `scripts/pi-session-interop.mjs` — optional explicitly gated Pi `SessionManager` conformance probe.
- Modify `README.md` — Pi v3 breaking change, storage/interoperability, CLI behavior, and TUI picker controls.
- Modify `AGENTS.md` only if new test commands or enforced behavior need repository guidance.

---

### Task 1: Add Pi v3 Wire Types, Bounded Codec, and Golden Fixtures

**Execution profile:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.

**Files:**
- Create: `internal/session/pi_types.go`
- Create: `internal/session/pi_codec.go`
- Create: `internal/session/pi_codec_test.go`
- Create: `internal/session/testdata/pi-v3/linear.jsonl`
- Create: `internal/session/testdata/pi-v3/tree.jsonl`
- Create: `internal/session/testdata/pi-v3/compacted.jsonl`
- Create: `internal/session/testdata/pi-v3/unknown-entry.jsonl`

**Interfaces:**
- Consumes: Pi v3 schema documented in the approved spec.
- Produces: `decodePiFile(io.Reader) (piFile, []Warning, error)`, `decodePiHeader([]byte)`, `decodePiEntry([]byte)`, `encodePiRecord(any)`, `piHeader`, `piEntry`, `piMessage`, and typed size/format errors used by Tasks 2–4.

- [ ] **Step 1: Write failing codec and fixture tests**

Create tests that load exact LF-delimited fixtures and assert field names, raw unknown preservation, and size failures:

```go
func TestDecodePiV3LinearFixture(t *testing.T) {
    file, err := os.Open("testdata/pi-v3/linear.jsonl")
    if err != nil { t.Fatal(err) }
    defer file.Close()

    decoded, warnings, err := decodePiFile(file)
    if err != nil { t.Fatal(err) }
    if len(warnings) != 0 { t.Fatalf("warnings = %#v", warnings) }
    if decoded.Header.Type != "session" || decoded.Header.Version != 3 || decoded.Header.CWD != "/workspace" {
        t.Fatalf("header = %#v", decoded.Header)
    }
    if len(decoded.Entries) != 4 || decoded.Entries[0].ParentID != nil {
        t.Fatalf("entries = %#v", decoded.Entries)
    }
}

func TestDecodePiV3PreservesUnknownEntryRawJSON(t *testing.T) {
    decoded := readPiFixture(t, "unknown-entry.jsonl")
    entry := decoded.Entries[1]
    if entry.Type != "future_entry" || !bytes.Contains(entry.Raw, []byte(`"futureField"`)) {
        t.Fatalf("entry = %#v", entry)
    }
}

func TestDecodePiV3RejectsOversizedEntry(t *testing.T) {
    _, _, err := decodePiFile(strings.NewReader(strings.Repeat("x", maxSessionEntryBytes+1)))
    if !errors.Is(err, ErrSessionEntryTooLarge) { t.Fatalf("error = %v", err) }
}
```

Fixtures must use Pi's exact `session`, `message`, `custom`, `model_change`, `compaction`, and `parentId` casing; do not generate fixtures from Otto structs.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
go test ./internal/session -run '^TestDecodePiV3' -count=1
```

Expected: compilation fails because Pi codec/types are undefined.

- [ ] **Step 3: Implement exact wire structs and bounded codec**

Define a raw-preserving envelope:

```go
const (
    PiSessionVersion    = 3
    maxSessionEntryBytes = 16 << 20
    maxSessionFileBytes  = 256 << 20
)

type piHeader struct {
    Type          string  `json:"type"`
    Version       int     `json:"version"`
    ID            string  `json:"id"`
    Timestamp     string  `json:"timestamp"`
    CWD           string  `json:"cwd"`
    ParentSession *string `json:"parentSession,omitempty"`
}

type piEntryBase struct {
    Type      string  `json:"type"`
    ID        string  `json:"id"`
    ParentID  *string `json:"parentId"`
    Timestamp string  `json:"timestamp"`
}

type piEntry struct {
    piEntryBase
    Raw json.RawMessage
    Message *piMessage
    // typed payload pointers for supported non-message entries
}
```

Use `bufio.Reader.ReadSlice('\n')` or an equivalent bounded reader; count total bytes before allocation, accept a final record without LF, and return typed errors for entry/file limits. Strictly validate supported payload fields while retaining raw JSON for unknown entry types.

- [ ] **Step 4: Run focused and package tests**

```bash
go test ./internal/session -run '^TestDecodePiV3' -count=1
go test ./internal/session -count=1
```

Expected: PASS; existing private-v1 store behavior remains unchanged in this task.

- [ ] **Step 5: Commit**

```bash
git add internal/session/pi_types.go internal/session/pi_codec.go internal/session/pi_codec_test.go internal/session/testdata/pi-v3
git commit -m "feat: add Pi v3 session codec"
```

---

### Task 2: Replace Persistent Store with Pi v3 Append-Only Tree Storage

**Execution profile:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.

**Files:**
- Modify: `internal/session/types.go`
- Modify: `internal/session/store.go`
- Modify: `internal/session/store_test.go`
- Modify: `internal/session/memory.go`
- Delete: `internal/session/persisted.go`
- Modify: `cmd/otto/main.go` (new session version only)
- Modify: `cmd/otto/main_test.go` (fixtures that assert the old header)

**Interfaces:**
- Consumes: Pi codec/types from Task 1.
- Produces: `CurrentVersion`, `ErrUnsupportedSessionFormat`, `ErrInvalidSession`, `ErrSessionEntryTooLarge`, `ErrSessionFileTooLarge`, `RuntimeMetadata`, Pi-backed `Create`, `Open`, `ReadHeader`, `Store.Append`, `Store.Messages`, and unchanged `Session` ownership methods.

- [ ] **Step 1: Replace old-store tests with failing Pi v3 persistence tests**

Add tests before deleting production code:

```go
func TestCreateWritesPiV3HeaderAndOttoRuntimeEntry(t *testing.T) {
    header := Header{
        Version: CurrentVersion, ID: "550e8400-e29b-41d4-a716-446655440000",
        Workspace: t.TempDir(), Provider: "openai-compatible", Profile: "local",
        Model: "test-model", CreatedAt: time.Unix(1, 0).UTC(),
    }
    store, err := Create(t.TempDir(), header)
    if err != nil { t.Fatal(err) }
    defer store.Close()

    lines := readJSONLines(t, store.Path())
    assertJSONEqual(t, lines[0], map[string]any{
        "type":"session", "version":float64(3), "id":header.ID,
        "timestamp":header.CreatedAt.Format(time.RFC3339Nano), "cwd":header.Workspace,
    })
    if !bytes.Contains(lines[1], []byte(`"customType":"otto.runtime"`)) {
        t.Fatalf("runtime line = %s", lines[1])
    }
}

func TestOpenRejectsOldOttoV1WithoutMutation(t *testing.T) {
    path := writeFixture(t, `{"type":"header","header":{"version":1}}`+"\n")
    before := readFile(t, path)
    _, _, err := Open(path)
    if !errors.Is(err, ErrUnsupportedSessionFormat) { t.Fatalf("error = %v", err) }
    if after := readFile(t, path); !bytes.Equal(after, before) { t.Fatal("old file mutated") }
}
```

Also port existing tests for permissions, `Sync` failures, incomplete-last-line truncation, complete-invalid preservation, defensive messages, close behavior, and dangling tool repair to Pi entries.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/session -run 'Test(CreateWritesPiV3|OpenRejectsOldOttoV1)' -count=1
```

Expected: FAIL because `Create` still writes the private header and old files are accepted as the native format.

- [ ] **Step 3: Implement Pi-backed create/open/append**

Keep the public domain header convenient for the rest of Otto:

```go
const CurrentVersion = PiSessionVersion

type RuntimeMetadata struct {
    Profile  string
    Provider string
    Model    string
}

type Header struct {
    Version int
    ID string
    Workspace string
    Provider string
    Profile string
    Model string
    CreatedAt time.Time
}
```

`Create` writes and syncs the Pi header, then writes an `otto.runtime` custom entry with `parentId:null`. If the second write fails, close and remove the incomplete new file. `Append` maps the provider-neutral message to a Pi `message` entry, generates a collision-free eight-hex entry ID, sets parent to the current leaf, writes+syncs, then updates memory.

`Open` uses Task 1's codec, validates base IDs/timestamps/parents, sets the final entry as leaf, derives domain header/runtime metadata, reconstructs messages, repairs dangling active-branch tool calls by appending error `toolResult` entries, seeks to EOF, and transfers file ownership only on full success.

Use `context.WithoutCancel` only for dangling-result durability, matching the agent's existing tool-result behavior.

- [ ] **Step 4: Update v3 creation call sites and remove private wire types**

Change new session creation to `Version: session.CurrentVersion`. Replace old JSON fixture helpers in `cmd/otto/main_test.go` with `session.Create` or Pi v3 test lines. Delete `internal/session/persisted.go` only after `rg 'persistedRecord|recordTypeHeader'` returns no production references.

- [ ] **Step 5: Run package and repository tests**

```bash
go test ./internal/session -count=1
go test ./cmd/otto -run 'TestRun(Resume|Continue|New)' -count=1
go test ./... -count=1
```

Expected: PASS with only Pi v3 files created by tests.

- [ ] **Step 6: Commit**

```bash
git add internal/session cmd/otto/main.go cmd/otto/main_test.go
git commit -m "feat: persist sessions as Pi v3 trees"
```

---

### Task 3: Build Active-Branch Context with Pi Message and Compaction Semantics

**Execution profile:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.

**Files:**
- Create: `internal/session/context.go`
- Create: `internal/session/context_test.go`
- Modify: `internal/session/store.go`
- Modify: `internal/session/pi_types.go`
- Modify: `internal/model/types.go`
- Modify: `internal/model/types_test.go`
- Modify: `internal/provider/openaicompat/protocol.go`
- Modify: `internal/provider/openaicompat/client_test.go`
- Modify: `internal/tui/entries.go`
- Modify: `internal/tui/entries_test.go`

**Interfaces:**
- Consumes: decoded ordered Pi entries and raw payloads from Tasks 1–2.
- Produces: `buildContext(entries, leafID) (ResolvedContext, []Warning, error)`, `ResolvedContext.Messages`, `ResolvedContext.Runtime`, `ResolvedContext.Usage`, `ResolvedContext.SessionName`, `ResolvedContext.ThinkingLevel`, provider-neutral `model.RoleContext`, and deterministic context framing.

- [ ] **Step 1: Write failing branch/context tests from golden fixtures**

```go
func TestBuildContextUsesOnlyActiveRootToLeafPath(t *testing.T) {
    file := readPiFixture(t, "tree.jsonl")
    context, warnings, err := buildContext(file.Entries, file.Entries[len(file.Entries)-1].ID)
    if err != nil { t.Fatal(err) }
    if len(warnings) != 0 { t.Fatalf("warnings = %#v", warnings) }
    assertMessageTexts(t, context.Messages, []string{"root", "active branch"})
}

func TestBuildContextUsesRetainedTailCompactionCheckpoint(t *testing.T) {
    context := contextFromFixture(t, "compacted.jsonl")
    assertMessageTexts(t, context.Messages, []string{"[Compaction summary]\nsummary", "retained", "after"})
    if context.Usage.InputTokens != 12 || context.Usage.OutputTokens != 4 {
        t.Fatalf("usage = %#v", context.Usage)
    }
}

func TestBuildContextRejectsImageOnActiveBranchOnly(t *testing.T) {
    _, _, err := buildContext(entriesWithActiveImage(), "image-leaf")
    if !errors.Is(err, ErrUnsupportedSessionContent) { t.Fatalf("error = %v", err) }
    if _, _, err := buildContext(entriesWithInactiveImage(), "text-leaf"); err != nil { t.Fatal(err) }
}
```

Add cases for model change, latest `otto.runtime`, assistant fallback metadata, legacy `firstKeptEntryId`, branch summary, visible/hidden custom message, orphan root warning, multiple roots, unknown active entry warning, tool pairing, and Pi `pending` rejection.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/session -run '^TestBuildContext' -count=1
```

Expected: FAIL because tree-aware context construction does not exist.

- [ ] **Step 3: Extend provider-neutral context representation**

Define the session resolver result in `internal/session/context.go`:

```go
type ResolvedContext struct {
    Messages []model.Message
    Runtime RuntimeMetadata
    Usage model.Usage
    SessionName string
    ThinkingLevel string
}
```

Add context metadata without importing Pi types outside `internal/session`:

```go
const RoleContext Role = "context"

type Message struct {
    ID string
    Role Role
    Blocks []Block
    CreatedAt time.Time
    FinishReason FinishReason
    Usage *Usage
    ContextType string
    Display bool
}
```

Use deterministic text framing:

- Compaction: `[Compaction summary]\n<summary>`
- Branch summary: `[Branch summary]\n<summary>`
- Custom message: `[Custom context: <customType>]\n<text>`

`openaicompat.translateRequest` maps `RoleContext` to a wire `user` message. TUI conversion hides `Display:false`, and renders visible context as a neutral transcript entry rather than as a user's prompt.

- [ ] **Step 4: Implement tree traversal and compaction**

Build `id -> entry`, follow parent links with a visited set, reverse the path, then apply the latest active-path compaction. For `retainedTail`, emit summary plus materialized tail and post-compaction entries. For `firstKeptEntryId`, require the referenced entry on the active path and retain the documented range. Derive latest runtime/name/model changes while walking the same path.

Do not rewrite unknown entries or inactive branches. Preserve warning text as bounded static messages containing only sanitized IDs/types.

- [ ] **Step 5: Run focused, provider, TUI, and repository tests**

```bash
go test ./internal/session -run '^TestBuildContext' -count=1
go test ./internal/model ./internal/provider/openaicompat ./internal/tui -count=1
go test ./... -count=1
```

Expected: PASS; ordinary linear Otto conversations produce unchanged provider requests and transcript content.

- [ ] **Step 6: Commit**

```bash
git add internal/session/context.go internal/session/context_test.go internal/session/store.go internal/session/pi_types.go internal/model internal/provider/openaicompat internal/tui/entries.go internal/tui/entries_test.go
git commit -m "feat: build context from Pi session branches"
```

---

### Task 4: Add Read-Only Session Inspection and Recent-20 Listing

**Execution profile:** `openai-codex/gpt-5.4`, `high` thinking.

**Files:**
- Create: `internal/session/list.go`
- Create: `internal/session/list_test.go`
- Modify: `internal/session/types.go`
- Modify: `internal/session/store.go`

**Interfaces:**
- Consumes: Pi codec/context metadata from Tasks 1–3.
- Produces: `Inspect(context.Context, string) (SessionInfo, []Warning, error)` and `List(context.Context, root, workspace, currentPath string, limit int) (ListResult, error)`.

- [ ] **Step 1: Write failing listing tests**

```go
func TestListReturnsRecentValidWorkspaceSessionsWithoutMutation(t *testing.T) {
    root, workspace := makeSessionDirectory(t, 24)
    oldPath := writeOldOttoV1(t, root, workspace)
    corruptPath := writeCorruptJSONL(t, root, workspace)
    beforeOld, beforeCorrupt := readFile(t, oldPath), readFile(t, corruptPath)

    result, err := List(context.Background(), root, workspace, newestValidPath(t, root), 20)
    if err != nil { t.Fatal(err) }
    if len(result.Sessions) != 20 || result.Skipped != 2 { t.Fatalf("result = %#v", result) }
    if !result.Sessions[0].Current { t.Fatalf("first = %#v", result.Sessions[0]) }
    assertSortedNewestFirst(t, result.Sessions)
    assertBytesEqual(t, readFile(t, oldPath), beforeOld)
    assertBytesEqual(t, readFile(t, corruptPath), beforeCorrupt)
}

func TestListRejectsSymlinksAndOtherWorkspaces(t *testing.T) {
    root := t.TempDir()
    workspace := t.TempDir()
    valid := createPiSession(t, root, workspace, "valid")
    directory := filepath.Dir(valid)
    if err := os.Symlink(valid, filepath.Join(directory, "linked.jsonl")); err != nil { t.Fatal(err) }
    createPiSessionAtPath(t, filepath.Join(directory, "other.jsonl"), t.TempDir(), "other")

    result, err := List(context.Background(), root, workspace, "", 20)
    if err != nil { t.Fatal(err) }
    if len(result.Sessions) != 1 || result.Sessions[0].Path != valid || result.Skipped != 2 {
        t.Fatalf("result = %#v", result)
    }
}
```

Also test deterministic mtime ties, canonical path equality, context cancellation, empty directory, name-over-preview, runtime metadata, last active user text, no dangling-call repair, and limit validation.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/session -run 'Test(List|Inspect)' -count=1
```

Expected: compilation fails because listing APIs are undefined.

- [ ] **Step 3: Implement provider-neutral list types and read-only inspection**

```go
type SessionInfo struct {
    Path, ID, CWD, Name string
    Created, Modified time.Time
    MessageCount int
    LastUserText string
    Profile, Provider, Model string
    Current bool
}

type ListResult struct {
    Sessions []SessionInfo
    Skipped int
}
```

Compute the existing hashed workspace directory through a single exported/internal helper shared with `Create`. Use `os.ReadDir`, `Lstat`, regular-file checks, canonical current-path comparison, stat mtime sorting, and bounded read-only parsing. Continue candidates until 20 valid results, not merely the first 20 filenames.

- [ ] **Step 4: Run focused and race tests**

```bash
go test ./internal/session -run 'Test(List|Inspect)' -count=1
go test -race ./internal/session -count=1
```

Expected: PASS and no fixture mutation.

- [ ] **Step 5: Commit**

```bash
git add internal/session/list.go internal/session/list_test.go internal/session/types.go internal/session/store.go
git commit -m "feat: list Pi sessions for a workspace"
```

---

### Task 5: Add Transactional Resume to the Application Controller

**Execution profile:** `openai-codex/gpt-5.6-sol`, `xhigh` thinking.

**Files:**
- Modify: `internal/app/controller.go`
- Modify: `internal/app/controller_test.go`

**Interfaces:**
- Consumes: `session.ListResult`, `session.Session`, and warnings from Tasks 2–4.
- Produces: `ErrPersistenceDisabled`, `SessionBrowser`, `SessionReplacement`, `ResumeResult`, `SessionLister`, `ResumeFactory`, `WithSessionBrowser`, `Controller.ListSessions`, and `Controller.ResumeSession`.

- [ ] **Step 1: Write failing transactional lifecycle tests**

```go
func TestControllerResumeSwapsSessionRunnerAndRuntimeAtomically(t *testing.T) {
    old := newFakeSession("old")
    next := newFakeSession("next")
    nextRunner := &recordingRunner{}
    controller := newControllerWithBrowser(t, old,
        func(context.Context, int) (session.ListResult, error) { return session.ListResult{}, nil },
        func(context.Context, string) (SessionReplacement, error) {
            return SessionReplacement{
                Session: next, Runner: nextRunner,
                RuntimeInfo: RuntimeInfo{Provider:"openai-compatible", Profile:"next", Model:"next-model"},
            }, nil
        })

    result, err := controller.ResumeSession(context.Background(), next.Path())
    if err != nil { t.Fatal(err) }
    if len(result.Warnings) != 0 { t.Fatalf("warnings = %#v", result.Warnings) }
    if got := controller.Info(); got.SessionID != "next" || got.Profile != "next" || got.Model != "next-model" {
        t.Fatalf("info = %#v", got)
    }
    if old.closeCalls != 1 { t.Fatalf("old close calls = %d", old.closeCalls) }
}

func TestControllerResumeBuildFailureKeepsCurrentUsable(t *testing.T) {
    oldRunner := &recordingRunner{}
    controller := newControllerWithRunnerAndBrowser(t, newFakeSession("old"), oldRunner,
        nil, func(context.Context, string) (SessionReplacement, error) {
            return SessionReplacement{}, errors.New("build failed")
        })
    if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); err == nil {
        t.Fatal("expected resume failure")
    }
    if err := controller.Prompt(context.Background(), "still works", nil); err != nil { t.Fatal(err) }
    if oldRunner.calls != 1 { t.Fatalf("old runner calls = %d", oldRunner.calls) }
}

func TestControllerCloseWaitsForResumeWithoutDeadlock(t *testing.T) {
    entered, release := make(chan struct{}), make(chan struct{})
    controller := newControllerWithBrowser(t, newFakeSession("old"), nil,
        func(context.Context, string) (SessionReplacement, error) {
            close(entered)
            <-release
            return SessionReplacement{Session:newFakeSession("next"), Runner:&recordingRunner{}}, nil
        })
    resumeDone := make(chan error, 1)
    go func() { _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); resumeDone <- err }()
    <-entered
    closeDone := make(chan error, 1)
    go func() { closeDone <- controller.Close() }()
    close(release)
    select {
    case <-resumeDone:
    case <-time.After(time.Second): t.Fatal("resume timed out")
    }
    select {
    case <-closeDone:
    case <-time.After(time.Second): t.Fatal("close timed out")
    }
}
```

Cover current canonical path no-op, nil candidate session/runner, active prompt/new/resume rejection, canceled factory cleanup, cancellation after candidate but before close, old close fatal state, candidate close exactly once, factory callback reentrancy, list without configured browser, and concurrent close/race loops.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/app -run 'TestController(Resume|ListSessions|CloseWaitsForResume)' -count=1
```

Expected: compilation fails because browser/resume APIs are undefined.

- [ ] **Step 3: Define segregated optional frontend contract**

Avoid forcing noninteractive test backends to implement unused methods:

```go
type SessionBrowser interface {
    ListSessions(context.Context, int) (session.ListResult, error)
    ResumeSession(context.Context, string) (ResumeResult, error)
}

type SessionReplacement struct {
    Session session.Session
    Runner Runner
    RuntimeInfo RuntimeInfo
    Warnings []session.Warning
}

type ResumeResult struct { Warnings []session.Warning }

type SessionLister func(context.Context, int) (session.ListResult, error)
type ResumeFactory func(context.Context, string) (SessionReplacement, error)

func WithSessionBrowser(list SessionLister, resume ResumeFactory) Option
```

`Controller` implements `SessionBrowser`; when factories are absent methods return `ErrPersistenceDisabled`.

- [ ] **Step 4: Refactor replacement state machine and implement resume**

Generalize the existing `NewSession` phase protocol so new and resume share ownership/close/swap helpers. Never call factories, runner callbacks, session methods, or waits while holding `Controller.mu`. Treat context cancellation only before old-session close begins; once old close begins, complete or enter the fatal closed state.

Use canonical/clean path equality for current-path no-op without opening a candidate. `ListSessions` marks the current path on its defensive result copy so the injected lister does not capture the controller. Return defensive warning copies.

- [ ] **Step 5: Run focused stress, race, and package tests**

```bash
go test ./internal/app -run 'TestController(Resume|ListSessions|CloseWaitsForResume)' -count=20
go test -race ./internal/app -count=1
go test ./... -count=1
```

Expected: PASS without timeout, deadlock, duplicate close, or stale runtime info.

- [ ] **Step 6: Commit**

```bash
git add internal/app/controller.go internal/app/controller_test.go
git commit -m "feat: switch sessions transactionally"
```

---

### Task 6: Unify CLI Runtime Construction and Wire Pi Sessions

**Execution profile:** `openai-codex/gpt-5.4`, `high` thinking; escalate lifecycle failures to `gpt-5.6-sol xhigh`.

**Files:**
- Create: `cmd/otto/runtime_builder.go`
- Create: `cmd/otto/runtime_builder_test.go`
- Modify: `cmd/otto/main.go`
- Modify: `cmd/otto/main_test.go`

**Interfaces:**
- Consumes: session inspect/list/open and app replacement APIs from Tasks 2–5.
- Produces: `runtimeBuilder.resolveSession`, `runtimeBuilder.buildRunner`, `runtimeBuilder.openReplacement`, common registry construction, and controller browser injection.

- [ ] **Step 1: Write failing runtime-builder tests**

```go
func TestRuntimeBuilderUsesStoredProfileProviderAndModel(t *testing.T) {
    builder := newRuntimeBuilderForTest(t, configWithProfiles("default", "resumed"))
    metadata := session.RuntimeMetadata{Profile:"resumed", Provider:"openai-compatible", Model:"stored-model"}
    runtime, err := builder.resolveSession(metadata)
    if err != nil { t.Fatal(err) }
    if runtime.Profile != "resumed" || runtime.Model != "stored-model" || runtime.APIKey != "resumed-secret" {
        t.Fatalf("runtime = %#v", redactedRuntime(runtime))
    }
}

func TestRuntimeBuilderExternalPiUsesDefaultEndpointWithSessionModel(t *testing.T) {
    builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
    runtime, err := builder.resolveSession(session.RuntimeMetadata{
        Provider:"openai-compatible", Model:"pi-model",
    })
    if err != nil { t.Fatal(err) }
    if runtime.Profile != "default" || runtime.Model != "pi-model" {
        t.Fatalf("runtime = %#v", redactedRuntime(runtime))
    }
}

func TestRuntimeBuilderFailureClosesCandidateStore(t *testing.T) {
    candidate := &trackedSession{Session:createPiStore(t), closed:make(chan struct{})}
    builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
    builder.openSession = func(string, string) (session.Session, []session.Warning, error) {
        return candidate, nil, nil
    }
    builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
        return nil, errors.New("runner failed")
    }
    if _, err := builder.openReplacement(context.Background(), candidate.Path()); err == nil {
        t.Fatal("expected replacement failure")
    }
    select {
    case <-candidate.closed:
    case <-time.After(time.Second): t.Fatal("candidate was not closed")
    }
}
```

Test missing named profile, unsupported provider, missing key, invalid endpoint, workspace mismatch, no secret in error, fresh tool registry limits/redaction, and warning propagation.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./cmd/otto -run '^TestRuntimeBuilder' -count=1
```

Expected: compilation fails because `runtimeBuilder` is undefined.

- [ ] **Step 3: Extract reusable construction**

Create a builder that owns immutable startup inputs only:

```go
type runtimeBuilder struct {
    config config.File
    environment map[string]string
    workspace *tool.Workspace
    workspacePath, sessionRoot, shell string
    noSession bool
    stderr io.Writer
    deps dependencies
    openSession func(path, workspace string) (session.Session, []session.Warning, error)
    buildRunnerOverride func(session.Session, config.Runtime) (app.Runner, error)
}

func (b runtimeBuilder) resolveSession(metadata session.RuntimeMetadata) (config.Runtime, error)
func (b runtimeBuilder) buildRunner(current session.Session, runtime config.Runtime) (app.Runner, error)
func (b runtimeBuilder) openReplacement(ctx context.Context, path string) (app.SessionReplacement, error)
```

The stored profile selects endpoint/key settings; stored provider/model are passed as explicit runtime overrides. For external Pi files without `otto.runtime`, use the default profile's endpoint/key source with session-derived provider/model. Redact secrets from all errors.

- [ ] **Step 4: Rewire startup, continue, resume, and new**

Replace `newestSessionPath` with `session.List(ctx, sessionRoot, workspacePath, "", 1)`. Use Pi v3 `Inspect` before runtime resolution. Configure `app.WithSessionBrowser` with:

```go
func(ctx context.Context, limit int) (session.ListResult, error) {
    return session.List(ctx, sessionRoot, workspacePath, "", limit)
}
builder.openReplacement
```

`Controller.ListSessions` marks the current canonical path on its defensive result copy. The lister therefore has no self-reference to an incompletely initialized controller.

For `--no-session`, do not configure browser factories; `/resume` receives `ErrPersistenceDisabled`.

- [ ] **Step 5: Run CLI and repository tests**

```bash
go test ./cmd/otto -run 'Test(RuntimeBuilder|RunResume|RunContinue|RunNew|NoSession)' -count=1
go test ./... -count=1
go test -race ./cmd/otto ./internal/app -count=1
```

Expected: PASS; startup behavior remains offline and Stage 1 only.

- [ ] **Step 6: Commit**

```bash
git add cmd/otto/runtime_builder.go cmd/otto/runtime_builder_test.go cmd/otto/main.go cmd/otto/main_test.go
git commit -m "refactor: share session runtime construction"
```

---

### Task 7: Add `/resume` Command and Asynchronous TUI State Lifecycle

**Execution profile:** `openai-codex/gpt-5.4`, `high` thinking; use `gpt-5.6-sol xhigh` for stale-generation or replacement races.

**Files:**
- Create: `internal/tui/resume.go`
- Create: `internal/tui/resume_test.go`
- Modify: `internal/tui/commands.go`
- Modify: `internal/tui/messages.go`
- Modify: `internal/tui/model.go`
- Modify: `internal/tui/model_test.go`

**Interfaces:**
- Consumes: optional `app.SessionBrowser` from Task 5.
- Produces: `slashCommandResume`, `resumePickerState`, `sessionListResultMsg`, `sessionResumeResultMsg`, `runSessionListCommand`, and `runSessionResumeCommand`.

- [ ] **Step 1: Write failing command and async-state tests**

```go
func TestResumeCommandLoadsRecentSessionsAsynchronously(t *testing.T) {
    backend := &resumeBackend{list: func(ctx context.Context, limit int) (session.ListResult, error) {
        if limit != 20 { t.Fatalf("limit = %d", limit) }
        return session.ListResult{Sessions: []session.SessionInfo{{ID:"old", Path:"/sessions/old.jsonl"}}}, nil
    }}
    m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 16)
    m.editor.SetValue("/resume")

    updated, cmd := m.Update(keyPress(tea.KeyEnter))
    loading := updated.(Model)
    if cmd == nil || loading.resume.mode != resumeLoading { t.Fatalf("state = %#v", loading.resume) }

    result := runCommandWithin(t, cmd, time.Second)
    updated, _ = loading.Update(result)
    loaded := updated.(Model)
    if loaded.resume.mode != resumeLoaded || len(loaded.resume.sessions) != 1 { t.Fatalf("state = %#v", loaded.resume) }
}
```

Add tests for command completion/help, active-turn rejection, no browser/persistence disabled, list error, empty/skipped state, stale list generation ignored, current selection no-op, resume success transcript/footer/usage replacement, success warning status, resume failure preserving all old UI/backend state, duplicate Enter blocked, and stale resume result ignored.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/tui -run 'TestResume(Command|Picker|Result|Success|Failure)' -count=1
```

Expected: FAIL because `/resume` and typed state/messages do not exist.

- [ ] **Step 3: Add command registry and typed worker messages**

```go
type resumeMode uint8
const (
    resumeClosed resumeMode = iota
    resumeLoading
    resumeLoaded
    resumeLoadError
    resumeResuming
    resumeResumeError
)

type resumePickerState struct {
    mode resumeMode
    generation uint64
    sessions []session.SessionInfo
    skipped, selected int
    err error
}

type sessionListResultMsg struct {
    generation uint64
    result session.ListResult
    err error
}

type sessionResumeResultMsg struct {
    generation uint64
    path string
    result app.ResumeResult
    err error
}
```

Workers return messages only; they never mutate the model or print to stderr.

- [ ] **Step 4: Implement Update-only lifecycle**

`/resume` clears the editor, verifies idle state and `app.SessionBrowser`, increments generation, sets loading, and returns list Cmd. Success stores defensive session copies. Enter on noncurrent selection sets resuming and returns resume Cmd. Resume success rebuilds `EntriesFromHistory`, usage, metadata-dependent footer, streaming fields, active channel, errors, scroll state, and picker state using the same complete reset discipline as `/new`.

Capture old transcript/usage until backend success. On error leave them untouched and keep the picker open. Every result checks generation and expected mode.

- [ ] **Step 5: Run focused, race, and package tests**

```bash
go test ./internal/tui -run 'TestResume(Command|Picker|Result|Success|Failure)' -count=10
go test -race ./internal/tui -run 'TestResume' -count=1
go test ./internal/tui -count=1
```

Expected: PASS without goroutine leaks or stale state mutation.

- [ ] **Step 6: Commit**

```bash
git add internal/tui/resume.go internal/tui/resume_test.go internal/tui/commands.go internal/tui/messages.go internal/tui/model.go internal/tui/model_test.go
git commit -m "feat: add TUI resume lifecycle"
```

---

### Task 8: Render and Control the Responsive Resume Picker

**Execution profile:** `openai-codex/gpt-5.4-mini`, `medium` thinking; escalate layout/key failures to `gpt-5.4 high`.

**Files:**
- Modify: `internal/tui/resume.go`
- Modify: `internal/tui/resume_test.go`
- Modify: `internal/tui/layout.go`
- Modify: `internal/tui/keymap.go`
- Modify: `internal/tui/model.go`
- Modify: `internal/tui/responsive_test.go`
- Modify: `internal/tui/security_test.go`

**Interfaces:**
- Consumes: `resumePickerState` and async lifecycle from Task 7.
- Produces: `renderResumePicker`, `resumeVisibleRange`, `formatRelativeSessionAge`, and modal exact-key routing.

- [ ] **Step 1: Write failing rendering and key tests**

```go
func TestResumePickerAtMinimumSizeShowsSelectionAndControlsWithinBounds(t *testing.T) {
    m := resizeModel(t, loadedResumeModel(t, 20), 40, 8)
    content := m.View().Content
    assertRenderedBounds(t, content, 40, 8)
    for _, text := range []string{"Resume", "1/20", "Enter", "Esc"} {
        if !strings.Contains(content, text) { t.Fatalf("view = %q, want %q", content, text) }
    }
}

func TestResumePickerNavigationPagesAndKeepsSelectionVisible(t *testing.T) {
    m := resizeModel(t, loadedResumeModel(t, 20), 80, 12)
    m = updateKey(t, m, tea.KeyDown)
    m = updateKey(t, m, tea.KeyPgDown)
    start, end := resumeVisibleRange(len(m.resume.sessions), m.resume.selected, resumeVisibleRows(m.width, m.height))
    if m.resume.selected < start || m.resume.selected >= end { t.Fatalf("range=%d:%d selected=%d", start, end, m.resume.selected) }
}
```

Add exact unmodified key tests, modified-key passthrough/swallow rules for a modal, Escape behavior in loading/loaded/error/resuming modes, current marker, loading spinner, empty/error/skipped text, resize transitions, long CJK/emoji/profile/path values, and terminal-control/entity injection.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./internal/tui -run 'TestResumePicker(AtMinimum|Navigation|Rendering|Sanitizes)' -count=1
```

Expected: FAIL because the picker renderer and modal key routing are incomplete.

- [ ] **Step 3: Add exact picker bindings and visible-window math**

Use `key.Binding` values for unmodified `up`, `down`, `pgup`, `pgdown`, `enter`, and `esc`. Clamp all selection math. Visible rows are `max(1, height - title/loading/help/border rows)` and never exceed available sessions. Page movement uses visible rows, not a magic constant.

Format age deterministically as `now`, `Nm`, `Nh`, `Nd`, `Nw`, `Nmo`, or `Ny`. Inject a clock into test state rather than calling time directly in assertions.

- [ ] **Step 4: Render bounded sanitized rows**

Rows prioritize, in order: cursor/current marker, age, profile/model, and title/last-user preview. Use `escapeSingleLineText`, ANSI-aware width, and ellipsis clipping. Never pass session metadata through Glamour. Show provider only when width permits.

While `resumeResuming`, ignore navigation/Enter; Escape reports `resume in progress` rather than hiding a pending result. Existing Ctrl+C process semantics remain first priority.

- [ ] **Step 5: Run TUI package and responsive stress tests**

```bash
go test ./internal/tui -run 'TestResumePicker' -count=10
go test -race ./internal/tui -count=1
go test ./internal/tui -count=1
```

Expected: PASS at all tested sizes with no line wider/taller than bounds.

- [ ] **Step 6: Commit**

```bash
git add internal/tui/resume.go internal/tui/resume_test.go internal/tui/layout.go internal/tui/keymap.go internal/tui/model.go internal/tui/responsive_test.go internal/tui/security_test.go
git commit -m "feat: render responsive session picker"
```

---

### Task 9: Add CLI/PTY Integration, Pi Interop Probe, and Documentation

**Execution profile:** `openai-codex/gpt-5.4-mini`, `medium` thinking; use `gpt-5.4 high` for PTY debugging.

**Files:**
- Modify: `cmd/otto/main_test.go`
- Modify: `cmd/otto/tui_pty_test.go`
- Create: `scripts/pi-session-interop.mjs`
- Modify: `README.md`
- Modify: `AGENTS.md` if the opt-in command is documented as a gate

**Interfaces:**
- Consumes: complete v3/list/controller/TUI behavior from Tasks 1–8.
- Produces: offline end-to-end proof, optional Pi-runtime conformance probe, and user-facing migration/resume docs.

- [ ] **Step 1: Write failing CLI and PTY integration tests**

Add a CLI test that creates two Pi v3 sessions, runs `--continue`, and asserts the newest valid session is chosen while a newer old-v1 file is skipped. Add a forced-TUI PTY scenario that types `/resume`, waits for `Resume Session`, sends Down/Enter, observes the selected transcript/session ID, resizes, and exits.

Use a fake backend/provider and temporary HOME; no credentials or network.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
go test ./cmd/otto -run 'TestRunContinueSkipsOldOttoSessions|TestTUIPseudoTerminalResumeLifecycle' -count=1
```

Expected: FAIL until all integration hooks and PTY fixture behavior are connected.

- [ ] **Step 3: Complete integration wiring**

Fix only integration defects revealed by the tests. Keep CLI OS signal ownership and `tea.WithoutSignalHandler()` unchanged. Ensure list/resume workers stop producing relevant messages after process cancellation and controller close waits without terminal corruption.

- [ ] **Step 4: Add optional Pi interoperability script**

`scripts/pi-session-interop.mjs` accepts a session path argument, imports `SessionManager` from an installed `@earendil-works/pi-coding-agent`, opens the file, builds context, and prints only bounded non-secret metadata. It exits with a clear skip code/message when Pi is not installed. It is never invoked by default Go tests.

Document an opt-in command such as:

```bash
OTTO_PI_INTEROP=1 node ./scripts/pi-session-interop.mjs /tmp/otto-session.jsonl
```

Do not add Node dependencies to `go.mod`, package manifests, or default CI.

- [ ] **Step 5: Update README accurately**

Document:

- Pi v3 compatibility baseline and separate Otto storage root
- Old Otto v1 files retained but unsupported/unlisted
- `/resume` recent-20 current-workspace scope and controls
- `--continue`/`--resume PATH` v3 behavior
- No `/tree`, `/fork`, naming, deletion, search, or non-Stage-1 provider claims
- Session files contain sensitive prompts/tool data but no credential fields

- [ ] **Step 6: Run integration and documentation checks**

```bash
go test ./cmd/otto -run 'TestRunContinueSkipsOldOttoSessions|TestTUIPseudoTerminalResumeLifecycle' -count=5
go test -race ./cmd/otto -run 'TestTUIPseudoTerminalResumeLifecycle' -count=3
test -z "$(gofmt -l .)"
git diff --check
```

Expected: PASS and alternate-screen enter/exit remains balanced.

- [ ] **Step 7: Commit**

```bash
git add cmd/otto/main_test.go cmd/otto/tui_pty_test.go scripts/pi-session-interop.mjs README.md AGENTS.md
git commit -m "docs: document Pi sessions and resume"
```

---

### Task 10: Whole-Branch Verification and Review Fixes

**Execution profile:** verifier `openai-codex/gpt-5.4 high`; final reviewer and any concurrency/security fixes `openai-codex/gpt-5.6-sol xhigh`.

**Files:**
- Modify only files required by evidence-backed final review findings.
- Do not add unrelated features or refactors.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: review-clean, fully verified branch ready for integration.

- [ ] **Step 1: Run the complete fresh gate suite**

```bash
test -z "$(gofmt -l .)"
go test ./... -count=1
go test -race ./... -count=1
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
go build -trimpath -o ./otto ./cmd/otto
go test ./cmd/otto -run '^TestTUIPseudoTerminalLifecycle$|^TestTUIPseudoTerminalResumeLifecycle$' -count=5
go test -race ./cmd/otto -run '^TestTUIPseudoTerminalLifecycle$|^TestTUIPseudoTerminalResumeLifecycle$' -count=3
git diff --check
rm -f ./otto
```

Expected: every command exits zero, no network/credential prompt, and the built binary is removed.

- [ ] **Step 2: Perform a whole-branch requirements review**

Review from the spec commit's parent through HEAD. Require explicit findings for:

- Pi v3 field/schema compatibility and unknown-entry preservation
- Tree/leaf/compaction/tool-result context correctness
- File bounds, symlink/workspace enforcement, secrets, and terminal sanitization
- Transactional candidate ownership, close/cancel races, and fatal persistence identity
- Stale Bubble Tea messages, picker modal precedence, and minimum-size bounds
- Old Otto v1 non-mutation and accurate docs

Critical and Important findings block completion.

- [ ] **Step 3: Fix each blocking finding with focused TDD**

For every finding: add one focused failing regression test, run it to observe the expected failure, implement the smallest fix, run focused tests, then rerun the affected package race tests. Commit fixes with an imperative `fix:` subject.

- [ ] **Step 4: Re-review only the fix diff, then rerun full gates**

A fresh reviewer verifies prior findings and checks the fix diff for new Critical/Important breakage. After a clean verdict, rerun Step 1 exactly and confirm `git status --porcelain` is empty.

- [ ] **Step 5: Record final commit state**

```bash
git status --short --branch
git log --oneline --decorate -15
git diff main...HEAD --stat
```

Expected: only intentional focused commits, clean tracked worktree, no `.superpowers/**` artifacts tracked, and no binary.
