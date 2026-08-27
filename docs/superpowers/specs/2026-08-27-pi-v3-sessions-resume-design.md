# Pi v3 Sessions and Interactive Resume Design

**Date:** 2026-08-27  
**Status:** Approved design  
**Compatibility target:** `@earendil-works/pi-coding-agent` 0.84.3 session format version 3

## 1. Summary

Replace Otto's private linear session format with a Pi-compatible version 3 JSONL session manager, then add a Pi-inspired `/resume` picker to the Bubble Tea TUI.

The implementation adopts Pi's documented session concepts instead of inventing a second session model:

- A versioned session header
- Append-only entries linked by `id` and `parentId`
- An active leaf whose ancestor path defines the active conversation
- Pi-compatible message, model-change, compaction, branch-summary, custom, label, and session-info entries
- Session listing, opening, context building, and transactional runtime switching

Otto remains a Go application using Bubble Tea. It does not embed Pi's TypeScript runtime or TUI. Compatibility is at the JSONL schema and behavioral-contract level.

This is an intentional breaking change. Existing private Otto v1 sessions are neither migrated nor opened by the new manager. They remain untouched on disk.

## 2. References

The design follows the installed Pi documentation and public source structure:

- Pi `docs/sessions.md`
- Pi `docs/session-format.md`
- Pi `core/session-manager`
- Pi `components/session-selector`
- Pi `AgentSessionRuntime` session-switch lifecycle

The compatibility baseline is Pi session version 3 as documented by package version 0.84.3. Future Pi format changes require an explicit Otto compatibility update rather than silent assumptions.

## 3. Goals

- Store all new persistent Otto sessions as Pi-compatible v3 JSONL.
- Let Pi explicitly open Otto-created session files when their messages use supported content.
- Let Otto open supported Pi v3 session files for the same canonical workspace.
- Preserve append-only history and unknown valid entries when appending.
- Build LLM context from the active root-to-leaf branch, including Pi compaction and branch-summary semantics.
- Add `/resume` to the TUI command registry.
- Show an asynchronous, responsive picker for the current workspace's 20 most recent valid sessions.
- Restore the selected session's transcript, usage, profile/provider/model, tools, provider client, and runner without restarting Otto.
- Keep the current session fully usable if listing, opening, runtime resolution, or runner construction fails.
- Keep default tests offline and deterministic.

## 4. Non-goals

- Migrating or importing Otto's old private v1 format
- `/tree`, `/fork`, `/clone`, `/name`, deletion, search, or compaction generation
- Listing sessions from other workspaces
- Supporting providers beyond Stage 1 `openai-compatible`
- Executing unsupported Pi image, thinking, provider, or tool content
- Embedding Pi's Node.js packages at runtime
- Guaranteeing compatibility with undocumented future Pi formats

## 5. Compatibility Contract

### 5.1 Supported Pi entries

Otto parses and indexes these Pi v3 entry types:

- `message`
- `model_change`
- `thinking_level_change`
- `compaction`
- `branch_summary`
- `custom`
- `custom_message`
- `label`
- `session_info`

Every non-header entry has:

```json
{
  "type": "message",
  "id": "a1b2c3d4",
  "parentId": "previous-id-or-null",
  "timestamp": "2026-08-27T08:00:00Z"
}
```

Unknown entry types with a valid base shape are retained as raw JSON, indexed in the tree, ignored for model context, and left untouched in the file. Otto emits a nonfatal warning when the active branch contains an ignored entry.

### 5.2 Unsupported content

Opening fails without changing the current session when the active branch requires content Otto cannot faithfully execute, including:

- Image content
- Unsupported assistant thinking content needed by the target provider
- Unsupported message roles
- A runtime provider other than `openai-compatible`
- A tool-call shape that cannot be represented by Otto's provider-neutral model

Unsupported content on an inactive branch remains preserved and does not prevent resuming the active branch.

### 5.3 Old Otto files

The previous Otto header shape is not Pi v3:

```json
{"type":"header","header":{...}}
```

The new parser returns a typed `ErrUnsupportedSessionFormat` for such a file. It does not rewrite, rename, truncate, or delete it. Session listing skips these files and reports the number skipped. Explicit `--resume` reports the unsupported-format error.

## 6. File Location and Identity

Otto continues to store sessions under:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

`workspace-key` remains the first 16 lowercase hexadecimal characters of SHA-256 over the canonical workspace path. Directories use mode `0700`; files use mode `0600`.

The storage location is Otto-specific, so Pi will not discover these files in Pi's default directory automatically. Pi can open an Otto session by explicit path.

A new header uses Pi's v3 shape:

```json
{
  "type": "session",
  "version": 3,
  "id": "uuid-v4",
  "timestamp": "2026-08-27T08:00:00Z",
  "cwd": "/canonical/workspace"
}
```

Session IDs are UUID v4 strings. Entry IDs are collision-checked, eight-character lowercase hexadecimal identifiers, matching Pi's documented convention.

## 7. Runtime Metadata

Pi headers deliberately do not contain Otto profile or credential configuration. Otto stores non-secret runtime selection in a standard Pi custom entry:

```json
{
  "type": "custom",
  "id": "a1b2c3d4",
  "parentId": null,
  "timestamp": "2026-08-27T08:00:00Z",
  "customType": "otto.runtime",
  "data": {
    "profile": "default",
    "provider": "openai-compatible",
    "model": "model-name"
  }
}
```

The entry is the first entry in a new Otto session. Pi can preserve and ignore it as normal extension state. Otto never stores API keys, OAuth tokens, authorization headers, or resolved secret values.

Runtime resolution for resume follows this order:

1. Latest `otto.runtime` entry on the active branch
2. Latest Pi `model_change` entry on the active branch
3. Provider/model from the latest assistant message on the active branch
4. Current config's default profile only as endpoint/credential source

When `otto.runtime.profile` is present, that named profile supplies endpoint, API-key environment name, timeouts, output limits, and other runtime settings. The stored provider/model remain the session selection. A missing profile, missing credential environment variable, invalid endpoint, unsupported provider, or missing model fails before session replacement.

External Pi sessions usually have no `otto.runtime` entry. Otto derives provider/model from Pi entries and uses the configured default profile as the endpoint and credential source. It rejects the resume if that profile cannot serve the derived Stage 1 provider/model.

## 8. Persisted Messages

Otto writes Pi AgentMessage-compatible message entries.

### 8.1 User

```json
{
  "type": "message",
  "id": "...",
  "parentId": "...",
  "timestamp": "...",
  "message": {
    "role": "user",
    "content": [{"type":"text","text":"Explain this repository"}],
    "timestamp": 1787817600000
  }
}
```

### 8.2 Assistant

Assistant entries include Pi-compatible:

- `role: "assistant"`
- text and `toolCall` content blocks
- `api: "openai-completions"`, `provider`, and `model`
- usage with input, output, cache, total, and cost fields
- terminal `stopReason`
- message timestamp in Unix milliseconds

Finish reasons map deterministically: Otto `stop` to Pi `stop`, `tool_calls` to `toolUse`, and `length` to `length`. An unknown/error terminal response uses Pi `error` with a sanitized `errorMessage`; a canceled terminal response uses `aborted`. Pi `pending` is rejected in persisted input because it is streaming-only.

Unsupported Pi usage fields are read and preserved; Otto-generated cache and cost fields are zero when the OpenAI-compatible endpoint does not report them.

### 8.3 Tool result

Tool results use Pi's `toolResult` message role, tool call ID, tool name, text content, error flag, and timestamp. Otto's existing bounded output and error semantics remain unchanged.

The `internal/session` adapter maps persisted Pi messages to and from `internal/model` without exposing Pi JSON structs to the agent or provider packages.

## 9. Append and Durability

The session file is append-only after creation.

For every header or entry write:

1. Encode exactly one JSON object.
2. Append one LF byte.
3. Call `Sync()` before reporting success.

Appending creates a child of the current leaf and advances the leaf. The first entry has `parentId: null`.

The manager maintains:

- Ordered raw entries
- Decoded supported entries
- `id -> entry` index
- Child index
- Current leaf ID
- Resolved labels and latest session name

On load, the last valid entry is the active leaf, matching Pi's append/branch model. The loader rejects duplicate IDs, invalid IDs, forward-parent references, cycles, invalid timestamps, and malformed supported entries. Multiple roots and entries whose parent is absent are retained as Pi-compatible orphan roots with warnings, matching Pi's tolerant tree view. Context building for an orphan active leaf starts at that orphan rather than inventing missing history.

An incomplete final JSON line is truncated only after all previous records validate, followed by `Sync()`, and produces a warning. A complete invalid final entry is never silently removed.

Unknown valid entries are never rewritten. New entries can safely append beneath them.

## 10. Context Building

`buildContextEntries()` walks parent links from the active leaf to the root, detects cycles defensively, and reverses the path.

`buildSessionContext()` follows Pi v3 behavior:

1. Determine the latest model and thinking-level changes on the active path.
2. Convert supported message entries into provider-neutral messages.
3. Treat a compaction with `retainedTail` as a self-contained checkpoint.
4. For older Pi compactions without `retainedTail`, honor `firstKeptEntryId` on the active path.
5. Convert `branch_summary` and `custom_message` entries into explicit provider-neutral context messages with deterministic, tested text framing.
6. Exclude plain `custom`, `label`, `session_info`, and unknown entries from LLM context.
7. Preserve tool-call/result ordering and pairing by tool call ID.

Transcript history uses the resolved active context path, while total session metadata such as message count can inspect all entries. Token usage displayed after resume is reconstructed from assistant usage on the active path, including supported compaction/branch-summary usage.

## 11. Session Listing

`internal/session` exposes provider-neutral metadata patterned after Pi's `SessionInfo`:

```go
type SessionInfo struct {
    Path          string
    ID            string
    CWD           string
    Name          string
    Created       time.Time
    Modified      time.Time
    MessageCount  int
    LastUserText  string
    Profile       string
    Provider      string
    Model         string
    Current       bool
}

type ListResult struct {
    Sessions []SessionInfo
    Skipped  int
}
```

Listing behavior:

- Scan only the current canonical workspace directory.
- Consider regular `.jsonl` files only; reject symlinks and non-regular files.
- Sort candidates by modification time descending, with path as a deterministic tie-breaker.
- Parse candidates until 20 valid Pi v3 sessions are collected or candidates are exhausted.
- Require exact canonical `cwd` equality.
- Use the latest `session_info` name when present; otherwise show the last active-branch user text.
- Mark the currently open canonical session path.
- Return a skipped count without exposing attacker-controlled raw record data in errors.

Listing is read-only. It never invokes interrupted-write repair or mutates files.

## 12. Application Controller

The provider-neutral application layer adds:

```go
type SessionReplacement struct {
    Session     session.Session
    Runner      Runner
    RuntimeInfo RuntimeInfo
}

type SessionLister func(context.Context, int) (session.ListResult, error)
type ResumeFactory func(context.Context, string) (SessionReplacement, error)
```

The frontend-facing backend gains:

```go
ListSessions(context.Context, int) (session.ListResult, error)
ResumeSession(context.Context, string) error
```

`ResumeSession` is transactional:

1. Reject closed, prompting, new-session, or resume-in-progress states.
2. Canonicalize and recognize selecting the current path as a no-op.
3. Build the complete replacement outside the controller lock.
4. Validate nonnil session and runner plus matching workspace.
5. If the context is canceled before replacement begins, close the candidate and keep current state.
6. Close the current session only after the candidate session, runtime, registry, provider client, and runner are ready.
7. Atomically swap session, runner, and runtime info under the controller lock.
8. If closing current fails, close the candidate and put the controller into the same fatal closed state used by existing replacement logic.

`Close` waits safely for an in-progress replacement without holding the mutex. `NewSession`, `ResumeSession`, and `Prompt` are mutually exclusive. Session-list loading is read-only but the TUI only starts it while idle.

The existing new-session replacement code should be refactored into shared internal replacement helpers rather than duplicating its lock protocol.

## 13. Command Wiring and Runtime Construction

`cmd/otto` remains responsible for concrete Stage 1 construction:

- Config and environment resolution
- Session root and canonical workspace
- Session opening
- OpenAI-compatible provider client
- Workspace tool registry
- Agent runner

Runtime-dependent provider/tool construction is extracted into a reusable function. The startup path, `/new`, CLI `--resume`, `--continue`, and TUI `/resume` all use the same construction rules.

For a selected session, the resume factory:

1. Reads and validates Pi v3 metadata without changing current state.
2. Resolves the session runtime from active-branch metadata and config.
3. Creates a fresh tool registry using that runtime's limits and redaction values.
4. Opens the persistent session.
5. Creates the provider client and agent runner.
6. Returns a complete `SessionReplacement` to the controller.

Warnings are returned to the frontend as structured result/status data; background workers must not print directly into the alternate screen.

`--continue` chooses the newest valid Pi v3 session for the workspace, skipping old Otto v1 and malformed candidates. `--resume PATH` reports explicit format/workspace/runtime errors.

## 14. TUI Resume Picker

`/resume` is added to the shared TUI command registry and help text. It accepts no arguments in this scope.

Opening it clears the editor and starts an asynchronous list command guarded by a generation number. Presentation state changes only in Bubble Tea `Update`.

Example:

```text
Resume Session

› current  2m   default/model-a   Fix the login flow
          18m   deepseek/model-b  Investigate test failures
           2d   default/model-a   Refactor configuration

↑/↓ select   PgUp/PgDn page   Enter resume   Esc cancel   1/20
```

Each row shows:

- Current-session marker
- Relative modification age
- Profile/model, with provider included when space permits
- Latest session name or last user-message summary

Controls:

- Up/Down: move selection
- PageUp/PageDown: move by visible page
- Enter: resume selected session
- Escape: close picker
- Ctrl+C: retain Otto's global cancel/clear/double-exit semantics

Selecting the current session closes the picker without rebuilding runtime.

Picker states are:

- Loading
- Loaded
- Empty
- Load error
- Resuming
- Resume error

While resuming, duplicate selection is disabled and a spinner is shown. On success the TUI:

- Rebuilds entries from the replacement active branch
- Reconstructs usage
- Updates footer metadata
- Clears streaming and stale turn state
- Closes the picker
- Scrolls to the transcript bottom
- Shows a concise resumed-session status

On failure it leaves the current transcript, footer, usage, session, and runner unchanged; the picker remains open with a retryable error.

Stale list or resume result messages cannot mutate a newer picker generation or a closed overlay.

The picker is responsive. At `40x8` it shows a title, at least one row, selection position, and essential controls. Larger terminals show more rows up to the available height. All external text uses the existing terminal-control sanitizers and width-aware clipping.

`/resume` is rejected while a turn, `/new`, or another resume is active. In `--no-session` mode it reports that session persistence is disabled.

## 15. REPL Scope

This feature adds the interactive picker only to the TUI. The line-oriented REPL does not gain an interactive `/resume` command because redirected streams cannot drive a full-screen picker safely.

CLI `--resume PATH` and `--continue` remain supported and move to the Pi v3 manager. REPL help must not claim `/resume` works interactively.

## 16. Security

- Canonical workspace equality is required before opening a session.
- Session listing rejects symlinks and non-regular files.
- Candidate paths must remain beneath the computed workspace session directory unless explicitly supplied via CLI `--resume`.
- JSON decoding is streaming and bounded: 16 MiB per JSONL entry and 256 MiB per session file. Exceeding either limit returns a typed size error rather than allocating unbounded memory.
- Picker text is sanitized as single-line untrusted metadata.
- Unknown fields and entries are never formatted into raw terminal output.
- Credentials are resolved from environment/config only and never persisted.
- Error messages do not include API keys, auth headers, or unbounded record content.
- Session files remain sensitive because prompts, tool arguments, and tool output are persisted.

## 17. Error Handling

Typed errors distinguish:

- Unsupported session format/version
- Invalid/corrupt session
- Workspace mismatch
- Unsupported active content/provider
- Missing runtime profile/credential
- Prompt or replacement active
- Persistence disabled
- Fatal persistence/close failure

Listing skips per-file format/corruption failures and counts them. A root-directory read failure fails the list operation. Explicit resume returns the selected file's error.

A failed candidate open or runner build closes every candidate resource. A successful swap transfers ownership to the controller exactly once.

## 18. Testing

All default tests stay offline.

### 18.1 Session conformance

Golden Pi v3 fixtures cover:

- Header and linear message chain
- Branched tree and active leaf
- Text and tool call/result messages
- Model and thinking-level changes
- Compaction with `retainedTail`
- Legacy Pi compaction with `firstKeptEntryId`
- Branch summary
- Custom and custom-message entries
- Labels and session info
- Unknown valid entry preservation

Tests assert Otto-generated JSON field names and shapes against the Pi 0.84.3 compatibility baseline.

An optional, explicitly gated interoperability script may open Otto fixtures through an installed Pi `SessionManager`; default `go test ./...` must not require Node, Pi, network access, credentials, or an interactive terminal.

### 18.2 Session reliability

Tests cover:

- Append/load/tree index
- Active branch context
- Parent and ID validation
- Duplicate, cycle, forward-parent, and malformed-entry rejection
- Multiple-root and orphan preservation with warnings
- Incomplete final-line recovery
- Complete-invalid-line preservation
- Write and `Sync` failure identity
- File and directory modes
- Defensive snapshots
- No credential fields
- Old Otto v1 typed rejection
- List ordering, limit, current marker, skipped count, workspace filtering, symlink rejection, and no mutation

### 18.3 Controller lifecycle

Tests cover:

- Successful list and resume
- Current-path no-op
- Candidate creation/build failure retaining current state
- Active prompt/new/resume rejection
- Resume cancellation before swap
- Old-close failure fatal state
- Candidate cleanup exactly once
- Resume versus close concurrency
- Runtime info changing atomically with session/runner
- Callback reentrancy and deadlock resistance

### 18.4 TUI

Tests cover:

- `/resume` registry completion and help
- Loading, loaded, empty, skipped, and error states
- Selection and paging
- Current-session no-op
- Resume success and retryable failure
- Transcript/footer/usage replacement
- Stale generation messages
- Active-turn and persistence-disabled rejection
- Minimum size, resize, clipping, metadata sanitization, and modal key precedence

### 18.5 Integration

Offline macOS PTY coverage opens `/resume`, selects a fixture session, verifies restored transcript/footer, resizes, and exits with alternate-screen restoration. Repeated race runs check for worker and channel leaks.

The full repository gates remain those documented in `AGENTS.md`.

## 19. Documentation

README updates must:

- Document Pi v3 as the only supported persistent format for new versions.
- State that old Otto v1 files are retained but not migrated or listed.
- Document `/resume` TUI controls and recent-20 scope.
- Document explicit-path interoperability and Otto's separate storage root.
- Avoid claiming support for `/tree`, `/fork`, compaction generation, unsupported providers, or full future Pi compatibility.

## 20. Delivery Sequence

Implementation should proceed in dependency order:

1. Pi v3 persisted types and conformance fixtures
2. Tree validation, append, load, and context building
3. Session listing and runtime metadata extraction
4. Transactional controller resume lifecycle
5. Shared CLI runtime/session construction
6. TUI command, picker, and responsive rendering
7. CLI/PTY integration and documentation
8. Full review and verification

No production behavior is added without a failing focused test first.
