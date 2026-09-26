# Sidebar follow-ups: remove a workspace, stop status reconnects on 401, refresh Changes after a turn

Status: approved 2026-09-27.

Three items left open by
[2026-09-26-web-sidebar-by-workspace.md](2026-09-26-web-sidebar-by-workspace.md),
[2026-09-26-serve-status-stream.md](2026-09-26-serve-status-stream.md), and
[2026-09-26-workspace-diff-review.md](2026-09-26-workspace-diff-review.md).
The scope was agreed on 2026-09-26; this document fixes the contracts.

Not in scope, by the same decision: two processes adding workspaces at once
can still lose one entry; an expired Bash approval is still not pushed; the
open session is still polled every 1s; the Changes view stays read-only.

## 1. Remove a workspace

Today a workspace added through `POST /v1/workspaces` stays loaded for the
life of the process and stays in `~/.otto/serve-workspaces.json` forever.

### Route

`DELETE /v1/workspaces?path=<path>`, same bearer token as every route.

- `path` is canonicalized the same way `admit_workspace` does, without the
  root check (a path loaded earlier may be outside a root that has since been
  removed from the config). A path that does not resolve is still matched
  literally against the persisted list, so an entry for a deleted directory
  can be removed.
- `204` when the workspace was unloaded, removed from the persisted list, or
  both.
- `404 WORKSPACE_NOT_FOUND` when it is neither loaded nor in the list.
- `409 WORKSPACE_IS_STARTUP` for the startup workspace.
- `409 WORKSPACE_IN_USE` when a session in that workspace is open in this
  process, or its workflow controller has an active run. The message names
  which. The client closes the sessions or waits for the runs first.

### Unloading

Under the `Workspaces.loaded` lock (the same lock `load` holds, so a
concurrent load or remove of the same path is serialized):

1. Re-check open sessions and active workflow runs; `409` if either.
2. Remove the entry from `loaded`.
3. `Controller::close().await` on its workflow controller, if any. With no
   active run this only closes the controller's executor.
4. Remove the path from the persisted list with the same temp-file-and-rename
   write as `add_to_workspace_list`.

Dropping the `WorkspaceHost` releases its builder and sandbox reloader. A
session in that workspace cannot be open (step 1), so nothing else holds
them.

`workflow::Controller` gains `active_runs(&self) -> usize` (the length of its
`active` map). `Factory` gains
`async fn remove_workspace(&self, path: &str) -> Result<(), WorkspaceRemoveError>`.
The open-session check needs `Server.sessions`, which `Factory` cannot see,
so the route checks open sessions before calling `remove_workspace`, and
`remove_workspace` checks active workflow runs under the lock. A session
opened in that workspace between the two checks is not excluded.
ponytail: the session check is outside the registry lock; move it under a
shared lock if a session opened during a removal becomes a real case.

### Web UI

Each non-startup group header gets a **Remove** button. It calls the route,
then refreshes the workspace and session lists. A `409` message is shown next
to the Add workspace field, like add errors. No confirmation dialog: removing
a workspace changes nothing on disk and it can be added again.

## 2. Stop reconnecting after a 401 on `/v1/status`

`useStatus` (`ui/src/status.ts`) reconnects every 1s after any error. After
a `401` (the server was restarted and issued a new token) it retries forever.

Change: when `streamStatus` throws `ApiError` with `status === 401`,
`useStatus` stops reconnecting and reports the error through a new
`onError` argument; `App` passes its existing `fail`. Other errors and a
normal stream end keep reconnecting after 1s.

## 3. Refresh the Changes view after a turn finishes

`ChangesView` fetches on open and on Refresh only.

Change: `App` passes the status map to `ChangesView`. When, for a session in
the view's workspace, `turn` changes from `running` to anything else between
two consecutive status maps, the view fetches again. The last-request guard
already in `ChangesView` keeps only the newest response.

## Tests (TDD order)

Rust:

1. `DELETE` of a loaded, added workspace with no sessions: `204`; it is gone
   from `GET /v1/workspaces` and from the persisted list.
2. `DELETE` of a path only in the persisted list (not loaded): `204`, removed
   from the list.
3. Unknown path: `404 WORKSPACE_NOT_FOUND`. Startup workspace: `409
   WORKSPACE_IS_STARTUP`.
4. With an open session in it: `409 WORKSPACE_IN_USE`; still loaded.
5. With an active workflow run: `409 WORKSPACE_IN_USE`; still loaded.
6. Without a token: `401`.
7. Persisted-list removal keeps the other entries and their order.

UI (vitest):

8. Remove button: shown for non-startup groups only; calls the API and
   refreshes; a `409` message is shown.
9. `useStatus` stops after a `401` and calls `onError`; it still reconnects
   after a non-401 error.
10. `ChangesView` refetches when a session in its workspace goes from
    `running` to `ok`, and not for a session in another workspace.

Docs: `testdata/server/openapi.yaml`; the user manual's `### Workspaces`,
`### HTTP API`, and `### Web UI` sections.

Gate: `make check`.
