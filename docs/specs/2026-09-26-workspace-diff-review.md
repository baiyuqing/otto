# Read-only review of a working directory's git changes

Status: approved 2026-09-26.

## Problem

The Web UI groups sessions by working directory
([2026-09-26-web-sidebar-by-workspace.md](2026-09-26-web-sidebar-by-workspace.md))
and shows their status
([2026-09-26-serve-status-stream.md](2026-09-26-serve-status-stream.md)), but
not what the sessions changed on disk. To review the changes, the user opens a
terminal and runs `git diff` in that directory. Facts from the current code:

- The `edit` tool returns a unified diff to the model only
  (`tool/edit.rs:609`, capped at 4096 bytes). No server route and no Web UI
  view shows file changes.
- Sessions in one working directory write the same files (no per-session
  worktree, by decision in step 2), so changes cannot be attributed to one
  session. The unit of review is the working directory.
- Otto already runs git for the prompt's `git: <branch>, N modified` line
  (`cli/workspace_context.rs`, `run_git`): through the workspace's sandbox
  `CommandExecutor` with the sandbox environment, `-c core.fsmonitor=false`,
  a deadline, and a cancel token.
- Each loaded workspace has a `WorkspaceHost` whose `Builder` holds
  `command_executor` and `sandbox_environment` (`cli/serve.rs:43`,
  `cli/runtime_builder.rs:644-646`). Both are `None` when the workspace has
  no usable sandbox.
- `GET /v1/sessions?workspace=<path>` resolves `workspace` through
  `Factory::load_workspace` (admission, load, persistence), with
  `workspace_load_error_response` for `400`/`403`.

## Scope

In scope: one read-only route that returns the working directory's changes
against `HEAD`, and a Web UI view that shows them.

Out of scope:

- Staging, committing, reverting, or editing. Nothing in this change writes to
  the repository or the working tree.
- Attributing changes to a session.
- Changes outside the working directory when it is a subdirectory of a
  repository; the pathspec is `.`.
- Ignored files (`.gitignore`).
- Automatic refresh. The view loads on open and on a **Refresh** click.
  ponytail: manual refresh; refetch on a status-stream turn finish in that
  directory if manual refresh proves insufficient.
- Syntax highlighting, side-by-side view, word-level diff.
- Sending the diff to the model. The route serves the authenticated HTTP
  client only.

## Server

### Route

`GET /v1/workspaces/diff?workspace=<path>`, same bearer token as every route.
`workspace` defaults to the startup workspace; when given, it goes through
`Factory::load_workspace`, as `GET /v1/sessions?workspace=` does, with the
same `400 INVALID_WORKSPACE` / `403 WORKSPACE_NOT_ADMITTED` responses.

`200` body:

```json
{
  "workspace": "/abs/path",
  "repository": true,
  "branch": "main",
  "files": [
    {"path": "src/a.rs", "old_path": null, "status": "modified",
     "binary": false, "patch": "@@ -1,3 +1,4 @@\n ...", "truncated": false}
  ],
  "truncated": false
}
```

- `repository: false` (with `branch: null`, `files: []`) when the directory
  is not inside a git work tree. This is a normal state, not an error.
- `branch` is `rev-parse --abbrev-ref HEAD`; `"HEAD"` when detached; `null`
  before the first commit.
- `status` is `modified`, `added`, `deleted`, `renamed`, or `untracked`.
  `old_path` is set only for `renamed`.
- `patch` is the file's hunks without the `diff --git` header lines; empty
  for a binary file (`binary: true`).
- Files are sorted by `path`; tracked and untracked files are in one list.

### Git commands

All run through the workspace's `CommandExecutor` in the workspace directory,
with the sandbox environment plus `GIT_OPTIONAL_LOCKS=0` (so `git status`-style
index refreshes do not write `.git/index`), and each with
`-c core.fsmonitor=false -c core.quotepath=off`:

1. `rev-parse --is-inside-work-tree`: failure means `repository: false`.
2. `rev-parse --abbrev-ref HEAD`: failure means no commit yet.
3. `diff <base> --no-color --no-ext-diff --no-textconv -M -- .`, where
   `<base>` is `HEAD`, or the empty tree
   `4b825dc642cb6eb9a060e54bf8d69288fbee4904` before the first commit. This
   covers staged and unstaged changes together.
4. `ls-files --others --exclude-standard -z -- .` for untracked files.
5. For each untracked file, `diff --no-index --no-color --no-ext-diff
   --no-textconv -- /dev/null <path>`; exit status `1` means "differs" and is
   success here.

`--no-ext-diff` and `--no-textconv` keep repository configuration from
running external programs; the sandbox bounds anything else git runs.

The server splits the output of 3 and 5 at each `diff --git ` line and reads
the file's status from the header lines (`new file mode`, `deleted file
mode`, `rename from`/`rename to`, `Binary files ... differ`). The splitter is
a pure function with its own tests.

### Limits

- Total `patch` bytes per response: 1 MiB. Files after the limit are listed
  with an empty `patch` and `truncated: true`, and the top-level `truncated`
  is `true`.
- One file's `patch`: 256 KiB, cut on a line boundary, `truncated: true`.
- Untracked files: the first 200 (by path) get a patch; the rest are listed
  with `truncated: true`.
- One deadline of 10 s for all commands. When it passes, the request fails
  with `504 git_timeout`.

### Errors

| Status | Code | When |
| --- | --- | --- |
| `400` | `INVALID_WORKSPACE` | as `load_workspace` |
| `403` | `WORKSPACE_NOT_ADMITTED` | as `load_workspace` |
| `501` | `diff_unavailable` | the workspace has no `command_executor` or sandbox environment (no usable sandbox; on Linux without `--sandbox off`) |
| `500` | `git_failed` | a command other than 1, 2, and 5's exit status `1` fails; the message carries git's exit status |
| `504` | `git_timeout` | the 10 s deadline passed |

### Factory

`Factory` gains
`async fn diff_runner(&self, workspace: &str) -> Option<(Arc<dyn CommandExecutor>, Vec<String>)>`,
returning the loaded host's executor and sandbox environment. The route logic
(commands, splitting, limits) lives in a new `server/diff.rs`, testable with a
recording executor like `workspace_context.rs`'s tests.

## Web UI

- `api.ts`: `getWorkspaceDiff(workspace)` and types `WorkspaceDiff`,
  `DiffFile`.
- Each sidebar group header gets a **Changes** button. It opens a Changes view
  for that directory in the main area (the same area the Chat, Usage,
  Workflows, and Agents views use).
- The Changes view shows the directory, the branch, a **Refresh** button, and
  one `<details>` per file: the summary line is status and path (`old → new`
  for a rename); the body is the patch in a `<pre>` with `+`, `-`, and `@@`
  lines styled. Binary files show "Binary file". Truncated files and a
  truncated response show a note. `repository: false` shows "Not a git
  repository". An error response shows its message.
- Nothing in the view writes to the server.

## Tests (TDD order)

Rust:

1. Splitter: modified, added, deleted, renamed, binary, and several files in
   one diff output produce the expected entries.
2. Per-file 256 KiB cut on a line boundary; total 1 MiB cut marks later
   files `truncated`.
3. Route with a recording executor: command argv, working directory, and
   environment (including `GIT_OPTIONAL_LOCKS=0`) are as listed above.
4. Not a work tree gives `repository: false`, `files: []`.
5. No commit yet diffs against the empty tree and gives `branch: null`.
6. Untracked files appear with `status: "untracked"`; `diff --no-index`
   exit status `1` is accepted.
7. No executor gives `501 diff_unavailable`; without a token, `401`.
8. An unadmitted `workspace` gives `403 WORKSPACE_NOT_ADMITTED`.
9. A git failure gives `500 git_failed`.
10. One test against a real temporary git repository (skipped when `git` is
    not on `PATH`), through the direct driver: a modified, an added, and an
    untracked file are reported.

UI (vitest):

11. The Changes button in a group header opens the view for that directory.
12. The view renders files, statuses, a rename, a binary file, the truncated
    note, and "Not a git repository".
13. Refresh fetches again.

Docs: `testdata/server/openapi.yaml` gets the route; the user manual's
`### HTTP API` and `### Web UI` sections describe it.

Gate: `make check`.
