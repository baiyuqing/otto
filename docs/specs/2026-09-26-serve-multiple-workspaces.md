# One `otto serve` process for several workspaces

Status: approved 2026-09-26.

## Problem

`otto serve` binds one workspace at startup (`--cwd`, default `.`). Every
session the process creates or resumes runs in that directory. Working on two
repositories from the web UI needs two server processes, two URLs, and two
tokens, and only the Agents view (`GET /v1/tasks`, backed by
`~/.otto/tasks.db`) shows both.

The binding is one `Builder` per process, shared by every session through
`ServeFactory { builder: Arc<Builder> }` (`crates/otto/src/cli/serve.rs:41`).
The workspace-dependent parts of `Builder` and of the serve composition are:

| Part | Where it is bound today | Workspace input |
| --- | --- | --- |
| File-tool root `&'static Workspace` | `cli/run.rs:379-391`, leaked once | canonical path |
| `workspace_path: String` | `cli/runtime_builder.rs:578` | canonical path |
| Sandbox executor behind one `SandboxSwitch` | `cli/run.rs:557-582` | `OpenOptions.workspace` |
| MCP config `builder.mcp` | `cli/run.rs:463-466` | server `cwd` default (`otto-core/src/config/mcp.rs:264`) |
| Memory scope `builder.memory.workspace_scope` | `cli/run.rs:665-666` | SHA-256 of the path, or `memory.workspace_ids` |
| Workflow controller and its `flock` | `cli/serve.rs:186-196`, `cli/workflow.rs:328-355` | `~/.otto/workflow-locks/{key}.lock` |
| Session listing and open | `cli/serve.rs:50-90` via `sessionfs::list(root, workspace_path)` | workspace directory under `~/.otto/sessions` |
| `GET /v1/info` `workspace` | `cli/serve.rs:201-210` | canonical path |

Parts that are already per session and derive from `workspace_path` on each
runner build: skill discovery (`cli/wiring.rs:363`), workspace context and
instructions (`cli/runtime_builder.rs:925-934`), MCP connections
(`runtime_builder.rs:895-899`), timers.

Parts that are already machine-wide and not tied to one workspace: the config
file, the provider runtime, ChatGPT auth, `usage.db`, `tasks.db`, the memory
database, the workflow database (its `workspace` column already exists,
`workflow.rs:31`).

## Scope

In scope: one serve process holds sessions in any number of admitted
workspaces; the HTTP API names the workspace when creating a session and
lists sessions and workflows per workspace.

Out of scope, each a later spec:

- Per-session git worktrees (step 2 of the desktop plan).
- A process-wide status stream and the project/session sidebar in the web UI
  (step 3). This spec adds only the minimum UI needed to pick a workspace for
  a new session.
- Diff review (step 4) and the desktop shell (step 5).
- TUI and REPL. They keep one workspace per process.
- Removing a workspace from a running process. A workspace stays loaded
  until the process exits.

## Admission: which directories a client may open

A workspace path arrives over HTTP, so it is validated at that boundary. The
token holder can already run `bash` and the file tools inside the startup
workspace; admitting an arbitrary path such as `/` or `$HOME` would extend
the file tools to all of it.

New config key:

```toml
[server]
workspace_roots = ["~/Work"]   # default: empty
```

A path is admitted when, after `canonical_directory` (the same function
`--cwd` uses), it is an existing directory and is either the startup
workspace or a descendant of one canonicalized `workspace_roots` entry. A
root itself is admitted. Symlinks are resolved before the check, so a link
inside a root that points outside it is rejected. Rejection returns `403`
with `code: "WORKSPACE_NOT_ADMITTED"`; a missing or non-directory path
returns `400` with `code: "INVALID_WORKSPACE"`.

With the default empty list, only the startup workspace is admitted, and the
server behaves as it does today.

## Server structure

Split `Builder` construction in `cli/run.rs` into:

- `Shared`: config, environment, provider runtime resolution, auth, usage,
  task recorder, memory `Service`, skill checker, session root. Built once.
- `Builder` for one workspace: `workspace`, `workspace_path`, sandbox
  switch and executor, MCP runtime, memory scope, plus an `Arc<Shared>`.
  Built by one function `Builder::for_workspace(shared, path)` that
  `cli::run` already calls once for the TUI, REPL, and serve.

`ServeFactory` replaces `builder: Arc<Builder>` with a registry:

```rust
struct Workspaces {
    startup: String,                                   // canonical path
    roots: Vec<PathBuf>,                               // canonical workspace_roots
    loaded: Mutex<BTreeMap<String, Arc<WorkspaceHost>>>,
}

struct WorkspaceHost {
    builder: Arc<Builder>,
    workflows: Option<Arc<workflow::Controller>>,      // None when the lock is held elsewhere
}
```

- The startup workspace is loaded at startup, as today.
- Another workspace is loaded on first use (a session create or a
  `/v1/workspaces` register), under the registry mutex so two concurrent
  requests for one path build one host.
- A failed load (sandbox open error, MCP config error) returns `500` with
  the redacted error and leaves nothing in the registry.
- The leaked `&'static Workspace` is leaked once per loaded workspace. The
  number of loaded workspaces is bounded by what the operator opens; there
  is no eviction (see Scope).

Each `app::Controller` receives the `Arc<Builder>` of its session's
workspace. `Controller::workspace()` then returns that builder's path with no
code change.

### Workflows

One `workflow::Controller` per loaded workspace, built with the existing
`build_controller`. Lock failure keeps today's behaviour at the workspace
level: the host's `workflows` is `None`, stderr gets `warning: workflows
disabled for <path>: <error>`, and `/v1/workflows?workspace=<path>` returns
the same error today's server returns when workflows are disabled.

### Sandbox reload

`POST /v1/sandbox/reload` reloads the sandbox of every loaded workspace and
keeps its current guard: `409` while any open session has a turn running.
Per-workspace reload is not added; the config it reloads is one file.

### Feishu inbound

Today inbound messages go to every open session, and every open session is
in the startup workspace. To keep that set unchanged, delivery is limited to
open sessions whose workspace is the startup workspace. An idle-session wake
follows the same rule.

## HTTP API changes

All additions are optional fields or parameters; existing clients see the
same responses when they do not send them.

| Route | Change |
| --- | --- |
| `GET /v1/workspaces` | New. `{"startup": path, "roots": [path...], "workspaces": [{"path", "open_sessions", "workflows": bool}]}` for loaded workspaces, startup first, then by path. |
| `POST /v1/workspaces` | New. `{"path": "..."}`. Admits and loads the workspace; `201` when newly loaded, `200` when already loaded. Returns one `workspaces` entry. |
| `POST /v1/sessions` | Optional `"workspace"` for a new session; default is the startup workspace. With `"resume"`, the session is searched in the named workspace, or in every loaded workspace when `workspace` is absent. |
| `GET /v1/sessions` | Optional `?workspace=`. Absent: sessions of every loaded workspace. Each session object gains `workspace` (canonical path). |
| `GET/POST /v1/workflows`, `GET /v1/workflows/{id}...` | Optional `?workspace=` on list and start, default startup workspace. Run-scoped routes find the run by id in any loaded workspace's controller. |
| `GET /v1/info` | Unchanged; `workspace` stays the startup workspace. |
| `GET /v1/tasks` | Unchanged; it already filters by `workspace`. |

Every `workspace` value passes admission before any lookup, so an
unadmitted path is `403` even for a read.

`openapi.yaml` is updated in the same change as each route.

## Web UI change in this spec

The **New session** control gains a workspace field: a select of
`GET /v1/workspaces` entries plus a text input that calls
`POST /v1/workspaces`. The session picker shows each session's workspace
basename. Anything beyond that belongs to step 3.

## Compatibility

- Session files are unchanged. The Pi v3 header `cwd` already records the
  workspace, and `validate_session_workspace` and `Prepared::prepare_listed`
  keep rejecting a session opened under the wrong builder; the registry
  selects the builder from the session's own directory, so those checks stay
  as they are.
- `workspace_roots` absent means one admitted workspace, so a server started
  with today's config and today's clients behaves as today.
- `tasks.db`, `workflows.db`, `usage.db`, and the memory database need no
  schema change.

## Implementation slices

Each slice is TDD: failing test, minimal change, focused run, then
`make check-fast`. `make check` runs after slice 5.

1. Split `Shared` and `Builder::for_workspace`; serve holds a registry with
   one entry. No behaviour change; existing tests are the guard, plus one
   test that two builders from one `Shared` have different workspace roots
   and sandbox workspaces.
2. `[server].workspace_roots` parsing and the admission function. Tests:
   startup path admitted; descendant of a root admitted; root admitted;
   sibling rejected; symlink out of a root rejected; missing path `400`;
   empty roots admits only the startup path.
3. `GET/POST /v1/workspaces` and the registry's load-once behaviour. Tests:
   concurrent `POST` for one path builds one host; failed load leaves the
   registry unchanged.
4. Sessions: `workspace` on create, list, resume, and in the session object.
   Tests: a session created in workspace B runs `ls` in B; `GET /v1/sessions`
   lists both; `?workspace=` filters; resume by id finds a B session;
   unadmitted path `403`.
5. Workflows per workspace, sandbox reload over all hosts, Feishu delivery
   limited to the startup workspace. Tests for each.
6. Web UI workspace picker, user manual `Agent server` section,
   `openapi.yaml`, and the AGENTS.md task map if module ownership moved.

## Open question

Should an admitted workspace outside `workspace_roots` also be addable from
the command line, e.g. `otto serve --workspace <path>` (repeatable), for
operators who do not want a broad root? This spec leaves it out; roots cover
the desktop case where the app registers folders the user picks under
their projects directory.
