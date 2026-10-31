# Session-owned runtime resources

Status: approved, 2026-10-31.

## Rule: a session owns its resources; a runner borrows them

A session is one transcript plus every resource whose identity is tied to that
transcript. A runner is the profile-specific composition used to run turns on
a session: provider client, tool registry, system prompt, agent options and
reviewer. A session can have several runners over its life; each runner
borrows the session's resources and never creates or closes them.

Resources of two different sessions are separate objects. Two runners of the
same session share one object per resource.

## Current state: runner-owned resources are duplicated or closed by a runner swap

`Builder::build_runner_inner` (`runtime/builder/runner_assembly.rs`) creates
these objects on every call, and `Runner::close` (`runtime/builder/runner.rs`)
closes them:

| Resource | Created at | Persisted to | Closed by |
| --- | --- | --- | --- |
| Sub-agent registry `Tasks` and its notification inbox | `build_subagents` (`runtime/wiring/catalogs_and_subagents.rs:102`) | `<session>.inbox.json` | `Agent::close` → `Tasks::close` |
| Timer registry `Reminders` | same | `<session>.reminders.json` | `Drop` |
| Bash output leases `Logs` | `runner_assembly.rs:17` | `.otto/tool-output/<id>.log` | runner drop |
| MCP `Servers` and connected clients | `connect_mcp_with_plugins` (`runtime/wiring/mcp_connect.rs:79`) | none | `Runner::close` spawns `Servers::close` |
| Workflow notification binding | `runner_assembly.rs:378` | none | `Runner::close` and `SharedSession::close` |

A runner is replaced on an open session by five paths: serve profile switch
(`app/session_ops.rs:57 switch_profile_result`), `/model` and profile switch
in the TUI/REPL (same function), `set_thinking` (`app/profile.rs:87`), and the
TUI background MCP swap (`cli/run/session_setup.rs:5` →
`app/runtime_control.rs:19 replace_runner_if_current`). Each replacement
builds a second set of the resources above for the same session and then
closes the first set. Observed consequences in the code (not reproduced by a
test yet):

1. `Runner::close` → `Agent::close` → `Tasks::close` cancels every running
   sub-agent of the session.
2. Each cancelled child pushes `TaskFinished` (`subagent/runner/child_run.rs:559`)
   into the old inbox. Both inboxes persist full snapshots to the same
   `<session>.inbox.json`; the old inbox's write replaces the file written by
   the new inbox.
3. The serve wake loop (`server/turn_runner.rs:20`) subscribes once, at
   session open, to the inbox and `Tasks` of the runner current at that time.
   When the old inbox is dropped, `updates.changed()` returns an error and the
   loop returns (`turn_runner.rs:49`). The session is no longer woken by
   sub-agent notifications.
4. Every replacement closes and restarts all stdio MCP server processes of the
   session, although the MCP configuration (`Builder::mcp`) does not depend on
   the profile.

Separately:

5. Child bash tools are keyed in the legacy `BashApprovals` by the empty
   string (`catalogs_and_subagents.rs:391`
   `with_approvals(String::new(), ..)`), while the parent uses the session id
   (`runner_assembly.rs:48`). `BashApprovals` is builder-level and shared by
   every session of a serve workspace, so all children of all sessions share
   key `""`.
6. `.otto/tool-output/` (bash logs, research scratch) and `.otto/reports/`
   hold files of every session in one directory.

## New data structures

### `SharedSession` becomes a handle to one `SessionState`

```rust
#[derive(Clone)]
pub struct SharedSession(Arc<SessionState>);

pub(crate) struct WeakSharedSession(Weak<SessionState>);

pub(crate) struct SessionState {
    transcript: Arc<dyn SessionHandle>,
    hooks: Arc<otto_core::agent::hooks::State>,
    resources: OnceLock<Arc<SessionResources>>,
}
```

This replaces the positional tuple `SharedSession(handle, hook_state,
workflow_binding_slot)` and the three-field `WeakWorkflowSession`. Call sites
that use `session.1` read `session.hooks()`.

### `SessionResources` holds every session-scoped runtime object

New module `runtime/session_resources.rs`:

```rust
pub(crate) struct SessionResources {
    session_id: String,
    /// Parent notification inbox, persisted to `<session>.inbox.json`.
    notifications: Arc<Inbox>,
    /// Sub-agent registry; `None` when `[agents]` is disabled.
    tasks: Option<Arc<Tasks>>,
    /// Timer registry; `None` when sub-agents are disabled (today's rule).
    reminders: Option<Arc<Reminders>>,
    /// Bash output leases under `.otto/tool-output/<session-id>/`.
    output_logs: Arc<Logs>,
    /// Connected once by the first runner build that connects MCP.
    mcp: tokio::sync::OnceCell<SessionMcp>,
    /// Durable-workflow notification binding (moved from `SharedSession.2`).
    workflow_binding: Mutex<Option<Arc<workflow_notifications::Binding>>>,
}

pub(crate) struct SessionMcp {
    servers: Arc<mcp::Servers>,
    connected: Vec<ConnectedServer>,
}
```

Creation: `Builder::session_resources(&self, session) -> Arc<SessionResources>`
is the only constructor. It runs inside `SessionState::resources.get_or_init`,
so the first runner build for a session creates the set and every later build
returns the same `Arc`. Inputs are workspace- or session-level only
(`task_recorder`, `no_session`, `[agents].enabled`, sidecar paths); nothing
depends on the profile. Loading `<session>.inbox.json`, the failover recovery
notice and `highest_task_counter` run once, here.

MCP: a runner build with `connect_mcp = true` calls
`resources.mcp.get_or_init(connect)`. Later builds create tool adapters from
`SessionMcp::connected` with that runner's `max_output`
(`mcp_child_tools` already does this for children). A build with
`connect_mcp = false` (TUI startup) leaves the cell empty and shows the
`connecting_mcp_servers()` status placeholder, as today.

### `Runner` keeps only profile-specific objects

`Runner` drops the fields `output_logs`, `workflow_binding` and `mcp`, and its
`tasks` and `reminders` fields become reads through
`self.session().resources()`. It keeps provider client, registry, prompt,
options, reviewer registration, memory binding and the sub-agent executor
`subagent::runner::Runner`. The executor is profile-specific (provider,
model, child tools) and holds an `Arc` of the session's `Tasks`; a child keeps
the executor objects it started with until it finishes.

`otto_core::agent::Agent::close` stops closing `Options::tasks`; it releases
the memory binding only. The task registry is closed by its owner.

## Lifecycle

| Event | Before | After |
| --- | --- | --- |
| Profile switch, `/model`, thinking change, background MCP swap | New tasks/inbox/MCP/logs; old set closed; children cancelled | Runner replaced; resources unchanged; running children continue on the profile they started with; new `agent` calls use the new profile |
| `/new`, `/load`, serve session close or idle sweep | Runner and session closed | `SharedSession::close` closes the resources: stop workflow binding, `Tasks::close`, drop reminders, spawn `Servers::close` |
| Process exit | `Runner::close_mcp().await` (5 s bound) | `SharedSession::close_resources().await` (same 5 s bound); `Runner::close_mcp` is removed |
| SIGTERM migration | `Runner::migrate` reads `self.tasks` | unchanged, reads the session's `Tasks` |

The serve wake loop needs no code change: `Controller::notification_mailbox`
and `subagent_tasks` return the session's objects, which a runner swap does
not replace.

## Child bash approvals are keyed by the owning session

`child_tools_with_output_logs` receives the session id and passes it to
`BashTool::with_approvals`. Children share the parent's key. This path is
used only when the scoped approval service is absent (`execute_inner(.., false)`,
`tool/bash/tool_impl.rs:282`); with the service, children are authorized by
`(session_id, task_id)` scope as today.

## Per-session directories under `.otto/`

| Files | Before | After |
| --- | --- | --- |
| Bash output logs | `.otto/tool-output/<log-id>.log` | `.otto/tool-output/<session-id>/<log-id>.log` |
| Research scratch | `.otto/tool-output/research-<hex>/` | `.otto/tool-output/<session-id>/research-<hex>/` |
| Sub-agent reports | `.otto/reports/<task-id>-<hex>/` | `.otto/reports/<session-id>/<task-id>-<hex>/` |

This separates names only. Workspace file tools and bash can read every
session's directory; it is not access control. The session id is checked to
be a single path component before use; `--no-session` uses its in-memory
header id. Existing files in the old locations are left in place; bash logs
there are already temporary.

## Unchanged

- `~/.otto/tasks.db` stays a process-wide index with one row per task carrying
  `parent_session`; it is read by `/agents` and `GET /v1/agents` only.
- Within a session, parent and children keep sharing the inbox, `ChildTools`,
  MCP clients and workspace files by design.

## Tests

Each test fails without the change; the failing assertion is named.

1. Profile switch keeps sub-agents: start a scripted child that blocks, call
   `switch_profile_result`, assert the child is still `Running` in
   `subagent_tasks()` (before: `Cancelled`).
2. Inbox file after switch: finish the child after the switch; assert
   `<session>.inbox.json` contains its `TaskFinished` and the new runner's
   `mailbox()` holds it (before: written by the old inbox, unseen by the new
   runner).
3. Serve wake loop after profile switch: push a notification after the switch
   and assert a wake turn runs (before: loop returned).
4. MCP survives a runner rebuild: two `build_runner_quiet` calls on one session
   return runners whose `mcp` is `Arc::ptr_eq` (before: two `Servers`).
5. Two sessions are separate: two sessions in one builder get different
   `SessionResources`, `Tasks` and inbox `Arc`s.
6. Child bash approval key equals the session id (before: `""`).
7. A bash log path contains `.otto/tool-output/<session-id>/`.

## Documentation

`docs/manual/interactive-use.md` (profile switch keeps sub-agents and MCP
connections; inbox section), `docs/manual/safety-and-tools.md` (output and
report paths), `docs/code-organization.md` (new module), CHANGELOG `Fixed` and
`Changed` entries.
