# Web UI sidebar: sessions grouped by working directory

Status: draft, awaiting approval.

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

- Server, HTTP API, `openapi.yaml`, session files. No change.
- Creating git worktrees, and sandbox access to a main repository's `.git`
  from a worktree directory. Today a session whose working directory is a
  worktree cannot write `<repo>/.git/worktrees/<name>` or `<repo>/.git/objects`
  under Seatbelt (`sandbox/seatbelt/profile.rs:181-186` grants only the
  working directory and Otto's private directories), so `git commit` fails
  there. That is a separate spec.
- Live per-session turn status across sessions (step 3's status stream). The
  sidebar shows only what `GET /v1/sessions` returns (`open`).
- Remembering added working directories across server restarts (see Open
  question).

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
`ui/src/app.css`, `docs/user-manual.md` (`### Web UI` and `### Workspaces`).

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

## Open question

The set of loaded working directories lives in the server process and is lost
on restart; after a restart the sidebar shows only the startup workspace until
directories are added again. Options, not part of this spec unless approved:

- A. Server-side: persist added paths (for example in `~/.otto/serve.toml` or
  a table in an existing database) and reload them at startup after
  admission.
- B. Browser-side: the UI keeps added paths in `localStorage` and re-posts
  them on load. Per browser only; the desktop shell would get it for free
  because it has one browser profile.

Recommendation: A, as a separate small spec, because the desktop shell and
any second client should see the same list.
