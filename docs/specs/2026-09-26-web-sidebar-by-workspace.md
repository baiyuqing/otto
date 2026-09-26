# Web UI sidebar: sessions grouped by working directory

Status: approved 2026-09-26, with persistence of added working directories included.

## Problem

After the multi-workspace server change
([2026-09-26-serve-multiple-workspaces.md](2026-09-26-serve-multiple-workspaces.md)),
one `otto serve` process holds sessions in several working directories. The
Web UI still shows them in one flat `<select>` in the top bar
(`ui/src/SessionPicker.tsx`), with the working directory reduced to a
basename suffix on each option. A second `<select>` picks the working
directory for **New session**. With more than a few sessions, finding the
sessions of one directory, or seeing which of them are open, requires reading
every option.

## Decision

The grouping key is the session's working directory: the `workspace` field
that `GET /v1/sessions` already returns for each session (the canonical path
recorded as the session header `cwd`). Otto does not create git worktrees and
does not relate two directories through git. A directory that happens to be a
git worktree is one more working directory and forms its own group.

## Scope

In scope: the chat view of the Web UI.

Out of scope:

- HTTP API shape, `openapi.yaml`, session files. No change. The only server
  change is persisting added working directories (see below).
- Creating git worktrees, and sandbox access to a main repository's `.git`
  from a worktree directory. Today a session whose working directory is a
  worktree cannot write `<repo>/.git/worktrees/<name>` or `<repo>/.git/objects`
  under Seatbelt (`sandbox/seatbelt/profile.rs:181-186` grants only the
  working directory and Otto's private directories), so `git commit` fails
  there. That is a separate spec.
- Live per-session turn status across sessions (step 3's status stream). The
  sidebar shows only what `GET /v1/sessions` returns (`open`).
- Removing a working directory, from the running process or from the
  persisted list. The persisted list only grows; see Persistence.

## Behaviour

The chat view gets a left sidebar that replaces `SessionPicker` in the top
bar.

- **Groups.** One group per working directory. The group list is the union of
  `GET /v1/workspaces` entries and the distinct `workspace` values of
  `GET /v1/sessions` rows, so a session row is never dropped when the two
  responses disagree. Order: the startup workspace first, then by path. The
  group header shows the basename, with the full path as the `title`.
- **Sessions in a group.** Rows whose `workspace` equals the group path, in
  the order the server returns them. Each row shows the session name (or
  `sessionLabel(id)`), `●` when `open` is true, and the model. The current
  session is marked with `aria-current="true"`. Clicking a row calls the
  existing `open(id)`.
- **New session.** Each group header has a **New session** button that calls
  `open(undefined, path)`, passing `undefined` for the startup workspace as
  today. The separate working-directory `<select>` is removed.
- **Add working directory.** The existing path input and **Add workspace**
  button move to the bottom of the sidebar, with the same
  `POST /v1/workspaces` call and error display. A successful add creates an
  empty group.
- **Refresh.** The sidebar reads sessions from the `sessions` state that
  `App.tsx` already refreshes (`refreshSessions`). Workspaces are fetched on
  mount and after a successful add.
- **Disabled state.** While `busy`, every button and row is disabled, as the
  picker is today.
- **Narrow screens.** Below 720px wide, the sidebar is hidden and a
  **Sessions** button in the top bar toggles it as an overlay. The page has
  no horizontal scroll at 360px.

The session chip in the top bar (name, working directory basename, Rename,
Context) stays.

## Implementation

Files: `ui/src/Sidebar.tsx` (new, replaces `SessionPicker.tsx`),
`ui/src/Sidebar.test.ts` (replaces `SessionPicker.test.ts`), `ui/src/App.tsx`,
`ui/src/app.css`, `crates/otto/src/cli/serve.rs` (persistence),
`docs/user-manual.md` (`### Web UI` and `### Workspaces`, including the
persisted list and its file path).

Grouping is one exported pure function,
`groupSessions(startup, workspaces, sessions)`, so it is tested without
rendering.

TDD order:

1. `groupSessions`: startup first then by path; a session whose workspace is
   not in the workspace list still gets a group; an added workspace with no
   sessions gets an empty group; rows keep server order.
2. `Sidebar` rendering: a row click calls `onOpen(id)`; a group's
   **New session** calls `onOpen(undefined, path)` and
   `onOpen(undefined, undefined)` for the startup group; the current session
   has `aria-current`; add-workspace success adds a group and failure shows
   the error; all controls disabled while `disabled`.
3. `App.tsx` wiring and CSS, including the narrow-screen toggle
   (`app.css.test.ts` for the media rule).
4. User manual.

Gate: `make check` (includes the UI tests and build).

## Persistence of added working directories

Without this, the set of loaded working directories lives only in the server
process; after a restart the sidebar shows only the startup workspace.

- **File.** `~/.otto/serve-workspaces.json`, `{"workspaces": ["/abs/path", ...]}`,
  canonical paths, sorted, no duplicates. One file per user, shared by every
  `otto serve` process.
- **Write.** After `POST /v1/workspaces` newly loads a workspace (the `201`
  case), the server reads the file, adds the path, and writes it to a
  temporary file in `~/.otto` followed by `rename`, while still holding the
  registry mutex. The startup workspace is not written. A write failure does
  not fail the request: the workspace stays loaded and stderr gets
  `warning: cannot save workspace list: <error>`.
- **Load at startup.** After the startup workspace is loaded, each listed path
  goes through `admit_workspace` with the current `workspace_roots`, then
  `Workspaces::load`. A path that is missing, not admitted, or fails to load is
  skipped with a stderr warning naming the path and the reason, and stays in
  the file. Startup does not fail because of the file. A missing file means an
  empty list; an unparsable file is a warning and an empty list, and it is not
  overwritten until the next successful add (which rewrites it from the
  empty list plus the new path).
- **Trust.** The file is not a way around admission: every entry is admitted
  against the running process's config, exactly as an HTTP request is.
- **Known limits.** Two serve processes adding at the same time can lose one
  entry (read-modify-write without a cross-process lock); the entry is
  restored the next time that directory is added. The list has no removal;
  a stale entry costs one warning line per start. Both are acceptable until a
  remove action is designed.

TDD (Rust, `crates/otto/src/cli/serve.rs` tests, offline, temporary home):

1. Add a workspace, restart the registry from the same home: the workspace is
   loaded and `GET /v1/workspaces` lists it.
2. A listed path outside `workspace_roots` after a config change is skipped
   with a warning; the other entries load.
3. A deleted directory is skipped with a warning; startup succeeds.
4. An unparsable file: startup succeeds with a warning; the file is unchanged.
5. The startup workspace is never written to the file; adding the same path
   twice leaves one entry.
