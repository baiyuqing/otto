# Otto embedding API

Status: proposed 2026-10-07. Maintainer decisions on 2026-10-07: another Rust
program can construct an Otto agent in process. The runtime it gets is by
default the same as the one the `otto` CLI builds. The host disables
individual components in code when it constructs the object.

## Problem

Today a program that wants an Otto agent has two options. Facts from this
checkout at `64fe51d`:

1. **Spawn `otto acp`** and speak ACP v1 over NDJSON on stdio. This is how
   `chat-connector/` uses Otto. The host gets a child process, a wire
   protocol, and `session/request_permission` as the only reverse request.
2. **Depend on the `otto` crate by git** and compose the runtime itself.
   The composition lives in `cli/run.rs::run_with_backends` (line 238, about
   850 lines). It runs 19 ordered steps:
   1. environment capture
   2. home resolution
   3. layered config and managed policy
   4. storage
   5. redaction boundary and credentials
   6. canonical workspace
   7. workspace marker
   8. shell resolution
   9. sandbox and MCP resolution
   10. usage, tasks, schedule, skill-checker and reflection stores
   11. approval service
   12. `Shared`
   13. `Builder::for_workspace`
   14. initial runtime
   15. workspace sandbox
   16. memory service
   17. runner
   18. `Controller`
   19. close order

   Several of these steps are `pub(super)` in `cli`. `load_workspace`
   (`cli/run.rs:1862`) is the reusable per-workspace part, and `otto serve`
   uses it. A host that copies this sequence must track every later change
   to it.

`otto_core::agent::Agent<P, T, S>` is public. It has no tools, sandbox,
session store or config loading, so it is not the Otto agent. The 2026-10-05
crate-split spec records that the `otto` crate "is not a published library".

The tests already build the runtime from code with components left out.
`cli/testutil.rs::shared_with_offline_sandbox` builds a `Shared` with:

- sandbox driver `off`
- `approval::Service::in_memory()`
- `no_session: true`
- usage, tasks, schedule and skill-checker set to `None`
- `Logger::discard()`

The internals already treat these components as optional. The missing piece
is a supported entry point.

## Goals

1. A Rust host constructs the agent with
   `otto::embed::Otto::builder()...build()`. The same workspace, config and
   environment give the same tools, sandbox, session store and policy as the
   `otto` CLI.
2. Every component in the [component table](#components) is enabled unless
   the host disables it. A disabled component:
   - adds no tool definitions
   - opens no database file
   - starts no child process
3. One `Otto` serves several sessions and several workspaces, as `otto serve`
   does.
4. The CLI, `otto serve`, `otto acp` and the embedding API run the same
   composition code. No second copy of `run_with_backends` exists.
5. The API surface covered by version compatibility is small and named:
   the `otto::embed` module and the `otto_core` types it exposes.

## Non-goals

- **Host-supplied implementations.** A host cannot plug in its own provider,
  tool executor or session store. It can only disable the built-in ones.
  Deferred until a host needs it.
- **Cargo features for compile-time trimming.** The whole `otto` crate,
  including TUI and server code, compiles into the host. Deferred.
- **Publication on crates.io.** This is phase 2 and is
  [described separately](#phase-2-publication-on-cratesio). Phase 1 is a git
  dependency.
- **Bindings for other languages** (Node, Python). Hosts in other languages
  use `otto acp`.
- **Enabling from code a component that config or managed policy disables.**
  A code switch only disables.
- **A code switch that runs bash without the sandbox.** Bash runs
  unsandboxed only through explicit `[sandbox].driver = "off"` in config,
  the same rule as the CLI.
- **More than one `Otto` per process.** See [one instance per
  process](#one-instance-per-process).
- **Inline config text.** A host writes a file and passes its path.
- **Session operations beyond the v1 list** in [API](#api): fork, archive,
  rename, profile switch, memory review and reflection commands.
- **A schedule worker in the host process.** See [Limitations](#limitations).

## Design

### API

```rust
use otto::embed::{ApprovalDecision, Component, ConfigSource, Input, Otto};

let otto = Otto::builder()
    .workspace("/path/to/repo")            // required
    .config(ConfigSource::Default)         // default; or ConfigSource::Path(..)
    .environment(std::env::vars())         // default: the process environment
    .overrides(overrides)                  // optional otto_core::config::Overrides
    .disable(Component::Mcp)               // any number of times
    .build()
    .await?;

for warning in otto.warnings() {
    eprintln!("{warning}");
}

let session = otto.new_session(None).await?; // None: the startup workspace
let stop = session
    .prompt(
        Input::Text("fix the failing test"),
        &mut |event| { /* otto_core::agent::Event */ },
        &cancel,
        |request| async move { ApprovalDecision::Deny },
    )
    .await?;
session.close().await;
otto.close().await;
```

`Otto` is `Clone + Send + Sync`. It wraps the shared state behind an `Arc`.

| Item | Maps to |
| --- | --- |
| `OttoBuilder::workspace(path)` | the startup workspace (`--cwd`) |
| `OttoBuilder::config(ConfigSource)` | `Default`: the CLI's layered load. `Path(p)`: the same as `otto --config p` |
| `OttoBuilder::environment(iter)` | replaces the environment snapshot, as the `cli::run` `environment_entries` contract does |
| `OttoBuilder::overrides(Overrides)` | `otto_core::config::Overrides`, the values of `--profile`, `--model`, `--provider`, `--base-url`, `--thinking` and related flags |
| `OttoBuilder::disable(Component)` | [component table](#components) |
| `OttoBuilder::build()` | `runtime::startup::start`, then `Builder::for_workspace` |
| `Otto::warnings()` | the startup warnings the CLI writes to stderr |
| `Otto::open_workspace(path)` | `admit_workspace` (`cli/serve.rs:243`), then `load_workspace`, the same as `POST /v1/workspaces` |
| `Otto::new_session(workspace)` | `Controller::create` (`app/mod.rs:2382`), as `ServeFactory::create` (`cli/serve.rs:481`) does |
| `Otto::resume_session(id, workspace)` | `Controller::open` (`app/mod.rs:2390`), as `ServeFactory::open` (`cli/serve.rs:501`) does; repair warnings are returned |
| `Otto::list_sessions(workspace, limit)` | `Controller::list_sessions` / `listed_for` |
| `Otto::close()` | the CLI close tail, see [Close](#close) |
| `Session::prompt(input, emit, cancel, decide)` | `Controller::run_with_approvals` (`app/approval.rs:95`) |
| `Session::steer(text)` | `Controller::steer` |
| `Session::compact(..)` | `Controller::compact` |
| `Session::history()` | `Controller::history` |
| `Session::info()` | `Controller::info` |
| `Session::close()` | `Controller::close_mcp`, then `Controller::close` |

Types:

- `Input` is `#[non_exhaustive]`. `Input::Text(&str)` maps to `Step::Text`
  and `Input::Image(&str, Block)` maps to `Step::Image`.
- `emit` is `otto_core::agent::EventSink`. Events are
  `otto_core::agent::Event`.
- `decide` receives `ApprovalRequest` and returns
  `Future<Output = ApprovalDecision>`. The module re-exports both from
  `app::approval`. A headless host passes a closure that returns `Deny`.
- `prompt` returns `Stop::EndTurn` or `Stop::Cancelled`.
- A second `prompt` on a session whose turn is still running returns the
  existing `PROMPT_ACTIVE` error. Sessions run turns concurrently with each
  other.

Errors are one `#[non_exhaustive] enum Error`. It has these variants:
`AlreadyRunning`, `Config`, `Workspace`, `SessionNotFound`,
`PersistenceDisabled`, `PromptActive`, `Closed` and `Agent(AgentError)`.
Every message passes through the builder's redaction (`redact_error`), as
serve does.

### Components

`Component` is `#[non_exhaustive]`, so adding a component is not a breaking
change. Each switch is applied in one of two ways:

- **Config key.** Where a config key already turns the component off, the
  switch sets that key to `false` on every `File` the runtime resolves. This
  happens after `ManagedPolicy::apply`, at startup and in `load_workspace`
  for each additional workspace. The disabled path is then the one the config
  key already takes and its existing tests cover.
- **Startup field.** Where no key exists, the switch leaves a `Shared` or
  `Builder` field empty. The table names the field.

| `Component` | Disabled effect | Applied through |
| --- | --- | --- |
| `Bash` | no `bash` tool, no sandbox executor, no sandbox reloader; workspace context skips `git status` (already gated on `bash_configured()`) | `Builder.command_executor = None` |
| `FileTools` | no read/write/edit/list/search file tools | `builtin_file_tools` returns no tools |
| `Web` | no `web_fetch`, no `web_search` (including the `chatgpt` hosted search) | `web_tools` returns no tools |
| `Mcp` | no MCP server process, no MCP tools | `[mcp].enabled = false` |
| `Memory` | memory service not opened (`Service::null(Disabled)`), no memory tools, no memory advisor | `[memory].enabled = false` |
| `Skills` | no skill catalog, no `skill` tool | `[skills].enabled = false` |
| `Plugins` | no plugin tools, no plugin hooks | `[plugins].enabled = false` |
| `Subagents` | no subagent tools (`build_subagents` returns empty) | `[agents].enabled = false` |
| `Reflection` | no reflect tool, no automatic reflection | `[reflection].enabled = false` |
| `Schedule` | `schedule.db` not opened, no schedule tools | `Shared.schedule = None` |
| `SessionPersistence` | no session files; sessions use `SharedSession::memory`; `resume_session` and `list_sessions` return `PersistenceDisabled` | `Shared.no_session = true` |
| `Usage` | `usage.db` not opened | `Shared.usage = None` |
| `TaskHistory` | `tasks.db` not opened | `Shared.task_recorder = None` |

The following are not switchable in v1. They are part of the turn loop or
the safety boundary:

- the approval service (`approvals.db`)
- `request_approval` and the approval-control tools
- `task_queue`, `update_plan`, the `otto` help tool and `list_models`
- redaction and managed policy
- the home directory and config lock

### Config, environment and secrets

- With `ConfigSource::Default`, `build()` reads the same files as the CLI:
  `~/.config/otto/config.toml`, the layered project config, then the managed
  policy at `managed_path()`.
- `ConfigSource::Path(p)` behaves as `otto --config p`.
- The managed policy is applied in both cases.
- The environment snapshot also decides the home directory. A host that
  wants a separate `~/.otto` sets the home variables in the snapshot it
  passes. No separate setter exists.
- There is no `api_key` setter. API keys come from the environment snapshot.
  ChatGPT credentials come from `otto login`, through the same
  `capture_auth_credentials` path as the CLI.

### Approvals and elevation

- `compose_workspace_sandbox` is called with `elevate = true`, as for
  `otto serve` and `otto acp`. Commands outside the sandbox's grants reach
  `decide` instead of failing outright.
- The logger defaults to `Logger::discard()`.
- Startup and runner warnings go to `Otto::warnings()` and
  `resume_session`'s return value. Runners are built with
  `build_runner_quiet`, as `Controller::create` already does
  (`app/mod.rs:2439`). The embedding API writes nothing to the host's
  stdout or stderr.

### One instance per process

The config file lock (`config/mod.rs:222`) is a POSIX `fcntl` lock. That
lock is held per process, not per file descriptor. A second `Otto` in the
same process would therefore pass the lock and race the first one on config
writes. It would also open the usage, tasks, schedule and approval stores a
second time.

`build()` sets a process-wide `AtomicBool` and returns
`Error::AlreadyRunning` when the flag is already set. `Otto::close()` and
the drop of the last `Otto` clone clear the flag.

A host that needs several repositories calls `Otto::open_workspace` for each
one. `otto serve` does the same thing through one `Shared`.

### Code moves

- **New `crates/otto/src/runtime/startup.rs`.** It holds steps 1–16 of
  `run_with_backends`, from environment capture to the memory service, as
  `pub(crate) async fn start(StartOptions) -> Result<Started, String>`. It
  also holds `load_workspace`, moved from `cli/run.rs`, and
  `admit_workspace`, moved from `cli/serve.rs`.
  - `StartOptions` carries the frontend kind, the config source, the
    environment snapshot, the overrides and the set of disabled components.
  - `Started` holds the `Arc<Shared>`, the startup `Builder`, the sandbox
    reloader, the warnings and the handles that the close order needs.
- **`cli/run.rs::run_with_backends`** calls `startup::start` and keeps only
  frontend dispatch: workflow, acp, serve, REPL/TUI, herdr and the schedule
  worker. `cli/serve.rs` calls `startup::load_workspace` and
  `startup::admit_workspace`.
- **New `crates/otto/src/embed.rs`.** It holds the public types above and
  depends on `runtime` and `app`. `crates/otto/tests/runtime_boundary.rs`
  already rejects imports of `cli`, `tui`, `acp`, `server` and `client` from
  every other module, and that test now covers `embed`.
- **`lib.rs`** adds `pub mod embed;`.

### Close

`Otto::close()` closes in the CLI's order:

1. every open `Session` (`close_mcp`, then `close`)
2. the sandbox control
3. the approval executor
4. the memory service
5. the process guard is released

`Session::close()` closes only that session. If the host drops an `Otto`
without calling `close`, the guard is still released, but the
approval-executor drain and the memory-service close are not awaited.

### Compatibility surface

Covered: `otto::embed` and the `otto_core` types its signatures name
(`agent::Event`, `agent::EventSink`, `config::Overrides`, `AgentError`). All
other `pub` items in `otto` stay `pub` for the frontends and tests, and they
carry no compatibility promise. The `otto::embed` rustdoc and
`docs/development.md` state this.

While the version is below 1.0, a breaking change to `otto::embed` bumps the
minor version and gets a `Changed` entry in `CHANGELOG.md`.

## Compatibility and safety

- **CLI, serve and acp.** Their behavior does not change. The move to
  `runtime/startup.rs` is a refactor. The existing CLI, serve and ACP tests
  are the regression check.
- **Session history.** The embedding API writes sessions through the same
  `Builder::create_session` and `Controller` code as the CLI. The
  append-only format is unchanged.
- **Trust boundaries.**
  - The workspace path is canonicalised as in the CLI.
  - An additional workspace is admitted only when it is the startup
    workspace, under a `workspace_roots` entry or under a trusted directory,
    after symlink resolution. This is the serve rule; `NotAdmitted`
    otherwise.
  - File tools keep their workspace-escape checks.
  - Bash keeps the native sandbox unless config says `driver = "off"`.
- **Secrets.** None are accepted as API arguments. The environment snapshot
  is the only source, as above.
- **Disable-only switches.** A host cannot weaken managed policy or turn on
  a component that config turns off.

## Testing

All tests are offline. They use `ProviderClient::Scripted`
(`runtime/builder.rs:433`), and sandbox driver `off` where the platform has
no Seatbelt.

1. **Default parity.** For one workspace and config, the tool definitions of
   a default `Otto` session equal those of the CLI composition. This catches
   drift between the two.
2. **Each switch.** One test per `Component`:
   - the tool names the table lists are absent
   - the database file it lists is not created under the test home
   - for `Mcp`, a configured stdio server is not spawned
   - for `SessionPersistence`, no `.jsonl` file is written
3. **Disable wins over config.** `[mcp].enabled = true` in config together
   with `disable(Component::Mcp)` gives no MCP tools. The same holds for a
   second workspace opened with `open_workspace`.
4. **Managed policy applies.** A managed policy that disables a component
   disables it with no code switch.
5. **One instance.** A second `build()` returns `AlreadyRunning`. A `build()`
   after `close()` succeeds.
6. **Prompt round trip.** A scripted provider requests a bash command that
   needs approval:
   - `decide` is called once with that command
   - `Deny` produces the denial tool result
   - the turn ends with `EndTurn`
   - the events arrive at `emit` in order
7. **Warnings.** A config that produces a known startup warning returns it
   from `Otto::warnings()`.
8. **Boundary.** `runtime_boundary.rs` passes with `embed.rs` present.
9. **Example.** The `otto::embed` module docs contain a `no_run` doc-test of
   the example above, so `cargo test --doc` compiles it.

## Documentation

- `otto::embed` rustdoc is the canonical reference.
- `docs/code-organization.md`: entries for `runtime/startup.rs` and
  `embed.rs`.
- `docs/development.md`: the compatibility surface, and the rule that
  `startup.rs` is the only composition root.
- `README.md`: a short "Use Otto as a library" section with the git
  dependency line, pinned to a release tag.
- `CHANGELOG.md`: an `Added` entry.
- Capability descriptions are unchanged, because no model-callable
  capability changes.

## Limitations

- Each loaded workspace calls `leaked_workspace` and keeps one `&'static
  Path` for the life of the process. `otto serve` has the same limit.
- `workspace::initialize` writes `.otto/workspace.json` into the host's
  workspace, as the CLI does.
- `~/.otto` is always opened, even with every component disabled. That
  covers storage, the config lock and `approvals.db`.
- No schedule worker runs in the host process. In the CLI the worker runs
  only for the REPL and TUI frontends (`cli/run.rs:1134`), so the host
  process behaves like `otto acp`. Schedule tools still create entries, and
  an `otto` REPL, TUI or `otto serve` process whose worker scope matches
  runs them.
- The API is Rust only and needs a Tokio runtime.
- Phase 1 is a git dependency. Builds of the host compile all of `otto`,
  including the TUI and server modules.

## Phase 2: publication on crates.io

Phase 2 is a separate spec and PR. It is listed here so that phase 1 does
not block it. These items must be resolved:

1. Remove `publish = false` from the workspace `Cargo.toml`. Give the
   `otto-core` path dependency a `version`.
2. Three `include_str!` paths point outside `crates/otto`, and
   `cargo package` does not include them:
   - `cli/storage.rs:262` → `scripts/export-legacy-storage.py`
   - `sandbox/seatbelt.rs:12` → `testdata/sandbox/profile_v1.sb`
   - `tool/otto.rs:11` → `docs/user-manual.md`

   Each file must move into the crate or be copied in at package time.
3. **Exact dependency pins.** `docs/development.md:449-451` requires `=x.y.z`
   pins. In a published library they force those exact versions into the
   host's dependency resolution and fail it on any conflict. Two options:
   - caret ranges plus the committed `Cargo.lock` for the binary, which
     changes the documented rule
   - keep exact pins and accept the conflicts
4. The release steps (`docs/development.md:729-744`) add
   `cargo publish -p otto-core`, then `cargo publish -p otto`.
5. Not checked: whether the names `otto` and `otto-core` are free on
   crates.io.

## Decisions

Resolved 2026-10-07 by the maintainer:

1. **In-process embedding for other Rust programs.**
2. **Default construction equals the CLI with everything enabled.** The host
   disables components in code at construction.

Proposed in this spec, accepted by approving it:

3. The component list and the way each switch is applied, as in the
   [component table](#components).
4. Switches only disable. There is no code switch that runs bash unsandboxed.
5. One `Otto` per process.
6. Config source is `Default` or `Path`. There is no inline config and no
   `api_key` setter.
7. No schedule worker in the host process, as for `otto acp`.
8. crates.io publication is phase 2, under its own spec.
