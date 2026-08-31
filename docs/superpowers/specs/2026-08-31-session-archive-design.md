# Session Archive Design

**Date:** 2026-08-31
**Status:** Proposed design (pending approval)
**Scope:** Stage 1, append-only Pi v3 sessions

## 1. Summary

Add a non-destructive **archive** operation that moves an active Pi v3 session file
out of the active session list into a sibling `archive/` directory:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl        (active)
~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl (archived)
```

Archiving is a pure filesystem move. It preserves the file byte-for-byte and its
`0600` mode, does not mutate the JSONL, and never deletes anything.

## 2. What "archiving" means

| Behavior | Before archive | After archive |
| --- | --- | --- |
| `/resume` picker | listed | **not listed** |
| `--continue` | may pick it | **never picks it** |
| `/archive` picker | listed as active | **not listed** |
| `--resume /path/to/archive/....jsonl` | — | **still works** |
| File bytes, mode, JSONL records | — | **unchanged** |
| Disk space | — | **not reclaimed** (not deletion) |

Archiving is **decluttering, not destruction**. It removes a finished session from
every active-session surface while keeping the file intact and explicitly
resumable by path.

Non-goals, matching Stage 1:

- Deletion, tree/forks, naming, or search.
- Compression or any transformation of the session file. A gzipped or renamed file
  would no longer be a resumable Pi v3 JSONL.
- Migration of old Otto v1 files.
- Any marker, label, or custom entry written into the session file. Archive state
  is expressed purely by location; writing an `otto.archive` entry would mutate the
  append-only history and Pi compatibility surface for no user benefit.

## 3. Why location, not a flag or a sub-folder name

`internal/session.List` already scans only the workspace-key directory for
top-level `.jsonl` entries and skips everything else by extension. A regular
`archive/` subdirectory is therefore **automatically** excluded from `/resume`,
`--continue`, and the `/archive` picker with zero changes to the listing logic.
The archive state is discovered from the filesystem, so a restart cannot forget
which sessions were archived.

Because `PrepareListed` (`internal/session/prepared.go`) validates that a listed
candidate is exactly `<root>/<key>/<basename>.jsonl` with a single-path component,
archived files can never be picked by the picker even if someone renames a file
into the wrong location.

Archived sessions remain resumable because `--resume PATH` (and `session.Prepare`)
open an explicit absolute path with no directory restriction; only workspace
equality is required. That behavior already exists and is unchanged.

## 4. Storage layer

### 4.1 API

```go
// internal/session
type ArchiveResult struct {
    Path string // new archive path
    ID   string // session ID
}

// Archive moves one active Pi v3 session file into the workspace's archive/
// directory. The source path must be a direct child of the workspace-key
// directory. On success the file exists only under archive/.
func Archive(ctx context.Context, root, workspace, path string) (ArchiveResult, error)
```

### 4.2 Archive steps

1. Honor `ctx` cancellation at each blocking step.
2. Reuse `validateListedCandidatePath(root, workspace, path)` to require the exact
   active shape `<root>/<key>/<basename>.jsonl` (single path component, `.jsonl`
   extension). This rejects `archive/` paths, foreign directories, and
   path-escapes by construction.
3. Open the candidate no-follow (rejecting symlinks and non-regular files) and
   `Inspect` it read-only, reusing the existing `internal/session` helpers. A
   session must be a valid Pi v3 session whose recorded `cwd` equals the current
   canonical workspace. This guarantees only List-visible active sessions can be
   archived and never corrupt/garbage files.
4. Immediately before the move, re-`lstat` the source path and require
   `os.SameFile(opened, current)`; fail if the path identity changed (same
   TOCTOU discipline as `verifyPreparedPathIdentity`).
5. Open the workspace-key directory no-follow. Create `archive/` under it with
   `unix.Mkdirat` + `openat(O_NOFOLLOW|O_DIRECTORY)` verification, mode `0700`.
   Reject a pre-existing `archive/` symlink or non-directory.
6. Verify the destination `<key>/archive/<basename>.jsonl` does not already exist
   (no-follow `lstat`) so a move can never overwrite an existing archived file.
7. `os.Rename` source to destination (same filesystem, atomic). Mode `0600` and
   file contents are preserved by the rename.

The operation does not need to open the file for writing; it reads for validation
and moves by path.

## 5. Controller layer (`internal/app`)

### 5.1 API

```go
type ArchiveFactory func(context.Context, string) (session.ArchiveResult, error)

func WithSessionArchiver(archive ArchiveFactory) Option

type SessionArchiver interface {
    SessionBrowser
    ArchiveSession(context.Context, string) (session.ArchiveResult, error)
    ArchiveCurrentSession() (session.ArchiveResult, error)
}
```

`SessionArchiver` extends `SessionBrowser` so the TUI can reuse one type assertion
for both pickers. The controller gains `ArchiveSession` and `ArchiveCurrentSession`
alongside `ListSessions`/`ResumeSession`.

### 5.2 ArchiveSession (non-current)

1. Canonicalize the path and reject closed/prompt-active/replacement states
   (`ErrPromptActive`/`ErrClosed`) and a missing archive factory
   (`ErrPersistenceDisabled`).
2. If the requested path equals the controller's current path, delegate to
   `ArchiveCurrentSession` so selecting "current" in the picker always behaves
   identically.
3. Otherwise call the injected `ArchiveFactory` directly. This is a read-free
   file move with no session or runner state, so no replacement lifecycle is
   involved.

### 5.3 ArchiveCurrentSession (current → archive + fresh)

`/archive` on the current session archives it and starts a fresh session, mirroring
`/new` after the move. It reuses the existing transactional replacement machinery
(`beginReplacementLocked` + `runReplacement`), so it inherits the same idle-state
rules, cancellation handling, and close-failure fatal-state behavior as `/new`.

The replacement build callback runs in dependency order that leaves **no partial
state on failure**:

1. Reject when persistence is disabled or the current session has no path
   (`ErrPersistenceDisabled`).
2. Build the fresh replacement exactly as `NewSession` does today (resolve runtime
   from the current `RuntimeInfo`, create session/runner, update runtime). If this
   fails, abort — nothing has moved.
3. Archive the current session file via the archive factory. If this fails, abort —
   the fresh candidate is closed and the current session is untouched and still
   fully usable.
4. Hand the ready replacement to `runReplacement`, which closes the current store
   (flushing to the already-moved file) and swaps atomically, replacing runtime
   info with the fresh session's (same as `/new`).

The build-first ordering means every failure path leaves the current session
completely intact; the only committed state change is the atomic file move, which
happens last.

`cmd/otto` wires `WithSessionArchiver` to a factory that resolves the session root
and canonical workspace and calls `session.Archive`, matching the existing
`WithSessionBrowser` wiring.

## 6. TUI `/archive` picker

`/archive` joins the TUI command registry and help. It opens a picker modeled on
`/resume` that lists the same active sessions (up to 20, current workspace, no
archived files because `List` excludes them).

- **Loading / Loaded / Empty / Load error / Archiving / Archive error** states,
  mirroring the resume picker's states.
- Same navigation keys as `/resume` (`↑`/`↓`, `PgUp`/`PgDn`, `Enter`, `Esc`).
- `Enter` on a **non-current** session archives it, closes the picker, and shows a
  concise status such as `archived session <id>`.
- `Enter` on the **current** session archives it and starts a fresh session, then
  closes the picker, rebuilds the transcript/footer from the fresh session, and
  shows `archived session; started new session`.
- The picker is rejected while a turn, `/new`, `/resume`, or another archive is
  active, and reports `persistence disabled` in `--no-session` mode.
- Stale list/archive result messages are ignored via the same generation guard as
  `/resume`.

Implementation keeps the resume picker untouched and adds a sibling archive picker
state + message handlers that reuse the shared sanitizing/rendering helpers
(`renderOverlay`, `renderResumeSessionRow`, `clipSingleLineText`,
`boundedSessionInfo`, `formatRelativeSessionAge`). The existing
`sessionListResultMsg` shape is reused for the archive list command.

## 7. REPL `/archive`

The line-oriented REPL cannot drive a full-screen picker, so `/archive` there
archives the **current** session and starts a fresh one (exactly
`ArchiveCurrentSession`), printing:

```text
Archived: /path/to/archive/<id>.jsonl
Session: <new-session-id>
```

`/archive PATH` is not added to the REPL; use the TUI picker or `--archive PATH`.
REPL help text gains the command line.

## 8. CLI `--archive PATH`

```bash
otto --cwd /path/to/project --archive /path/to/project-sessions/<id>.jsonl
```

`--archive PATH` archives one active session for the current canonical `--cwd`,
prints the new archive path, and exits `0`. It is a standalone utility that needs
only the home directory and workspace — no provider, model, or credentials.

Conflict rules:

- Cannot combine with `--continue`, `--resume`, `--no-session`, or `--approve`
  (all mutually exclusive with a standalone archive operation).
- Rejects paths that are not active sessions (including already-archived paths,
  foreign workspaces, symlinks, and invalid Pi v3 files) with typed errors.

## 9. Security

- All file access uses the existing no-follow (`O_NOFOLLOW`/`openat`) discipline;
  symlinked session files, workspace directories, session roots, and `archive/`
  directories are rejected.
- The candidate must be a direct child of the workspace-key directory and a valid
  Pi v3 session whose recorded workspace matches the current canonical workspace.
- The destination is checked non-existent before the atomic same-filesystem rename,
  so archiving never overwrites an existing file.
- No credentials, API keys, or auth headers are involved; archive paths and errors
  never carry session content.
- Archived files remain sensitive (they contain prompts, tool calls, tool output);
  the archive directory is mode `0700` and files keep mode `0600`.

## 10. Error handling

- Reuse typed errors where they fit: `ErrInvalidSession`, `ErrPromptActive`,
  `ErrClosed`, `ErrPersistenceDisabled`.
- Archive-specific failures: candidate changed between validation and move, archive
  directory is a symlink or non-directory, destination already exists, workspace
  mismatch, invalid/unreadable session.
- The TUI/REPL/CLI surfaces render archive errors exactly like their resume/new
  counterparts (sanitized, bounded, non-fatal to the current session).

## 11. Testing

TDD, all offline, per `AGENTS.md`.

**Storage (`internal/session`):**

- Archives a valid active session; bytes and mode preserved; file no longer under
  the active directory; `Inspect`/`Prepare` still open the archived path.
- `List` and `--continue` (which use `List`) no longer return archived sessions.
- Rejects: already-archived paths, foreign workspace, workspace mismatch, symlink
  source, non-regular source, invalid Pi v3, missing root/workspace dir, existing
  destination, context cancellation.
- No mutation of any file when validation fails.
- `archive/` created `0700`; existing `archive/` reused; symlinked `archive/`
  rejected; destination collision refused.
- Race/identity: source changed between inspection and move fails safely.

**Controller (`internal/app`):**

- `ArchiveSession`: success, current-path delegation, persistence disabled,
  prompt-active rejection, factory failure, closed state.
- `ArchiveCurrentSession`: success swaps to a fresh session and replaces runtime
  info; build failure retains current; archive failure retains current and closes
  the candidate; close failure reaches the fatal state; cancellation before swap;
  candidate cleanup exactly once; concurrency with close.

**TUI:**

- `/archive` registry, completion, and help.
- Picker loading/loaded/empty/error/archiving states, selection, paging, min-size,
  sanitization, stale-generation messages, active-turn and persistence-disabled
  rejection, current-session success (fresh view), non-current success (status),
  failure keeps the picker open and retryable.

**REPL:**

- `/archive` archives current and prints both paths; error wrapping as
  `commandError`; unknown-command behavior unchanged.

**CLI:**

- `--archive` success, conflicts, missing file, foreign workspace, already-archived
  path.

**Docs/registry tests:**

- `internal/tui/completion_test.go` enumerates commands and is updated for
  `/archive`.

## 12. Documentation

- `README.md`: add archive to the Stage 1 command/session lists and clarify it is
  not deletion.
- `docs/user-manual.md`: Sessions section documents `archive/`, `/archive` picker
  controls, REPL `/archive`, `--archive PATH`, and that archived sessions stay
  resumable via `--resume PATH`.

## 13. Delivery sequence

1. Storage `Archive` + tests (failing first).
2. `List`/`Prepare` coverage proving archived files are excluded yet explicitly
   resumable.
3. Controller `ArchiveSession`/`ArchiveCurrentSession` + tests.
4. CLI `--archive` + tests.
5. TUI `/archive` registry, picker, and handlers + tests.
6. REPL `/archive` + tests.
7. Docs, `completion_test.go` updates, PTY integration coverage.
8. Full repo gates (`make check`, race, staticcheck, diff check).

No production behavior is added without a failing focused test first.
