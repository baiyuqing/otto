# Otto embedding API

Status: approved 2026-10-07 (`67cc55e`); phase 1 implemented. Line
references point to `64fe51d`, before the code moves. Maintainer decisions
on 2026-10-07:

- Another Rust program can construct an Otto agent in process.
- The runtime it gets is by default the same as the one the `otto` CLI
  builds. The host disables individual components in code when it constructs
  the object.
- No breaking change. Existing behavior and public interfaces stay as they
  are, and the embedded runtime behaves the same as the CLI.

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
   `otto::embed::Otto::builder()...build()`. With no component disabled, the
   result behaves the same as `otto acp` started with the same `--cwd`,
   config and environment. That means the same tools, sandbox, approvals,
   session files, logs and warnings. See [behavior
   reference](#behavior-reference-otto-acp).
2. Every component in the [component table](#components) is enabled unless
   the host disables it. A disabled component:
   - adds no tool definitions, except `Schedule`, whose tools stay and
     report that scheduled delivery is unavailable
   - opens no database file
   - starts no child process

   Each disabled state is a state the CLI already reaches through a config
   key, a flag or an unavailable store. No switch creates a runtime state the
   CLI cannot produce.
3. One `Otto` serves several sessions and several workspaces, as `otto serve`
   does.
4. The CLI, `otto serve`, `otto acp` and the embedding API run the same
   composition code. No second copy of `run_with_backends` exists.
5. No breaking change. The following stay unchanged:
   - CLI, serve and ACP behavior
   - existing public Rust items, config, on-disk formats and wire protocols

   See [no breaking change](#no-breaking-change).
6. After it ships, `otto::embed` changes only by addition.

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
- **Switches for file tools and web tools.** The CLI has no state without
  them. Under goal 2 they wait until such a state exists. Adding them later
  is a new `Component` variant, which is not a breaking change.
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
    .diagnostics(Box::new(log_file))       // default: the process stderr, as `otto acp`
    .disable(Component::Mcp)               // any number of times
    .build()
    .await?;

let session = otto.new_session(None).await?; // None: the startup workspace
session.set_approval_mode(ApprovalMode::Manual)?; // approvals reach `decide`
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
| `OttoBuilder::diagnostics(writer)` | where warnings and log lines go. Default: the process stderr, which is where `otto acp` writes them |
| `OttoBuilder::build()` | `runtime::startup::start`, then `Builder::for_workspace` |
| `Otto::open_workspace(path)` | `admit_workspace` (`cli/serve.rs:243`, now `runtime/workspace_admission.rs`), then `load_workspace`, the same as `POST /v1/workspaces` |
| `Otto::new_session(workspace)` | `Controller::create` (`app/mod.rs:2382`), as ACP `session/new` does (`acp/mod.rs:1494`) |
| `Otto::resume_session(id, workspace)` | `Controller::open` (`app/mod.rs:2390`), as ACP `session/load` does (`acp/mod.rs:1520`); repair warnings are written as `warning: {redacted}` to the diagnostics writer, as ACP does |
| `Otto::list_sessions(workspace, limit)` | `Controller::list_sessions` / `listed_for` |
| `Otto::close()` | the CLI close tail, see [Close](#close) |
| `Session::prompt(input, emit, cancel, decide)` | `Controller::run_with_approvals` (`app/approval.rs:95`) |
| `Session::steer(text)` | `Controller::steer` |
| `Session::compact(..)` | `Controller::compact` |
| `Session::history()` | `Controller::history` |
| `Session::info()` | `Controller::info` |
| `Session::set_approval_mode(ApprovalMode)` | `approval::Service::set_mode` for the session scope, as ACP `_otto/approval_mode/set` and `/auto` do |
| `Session::set_auto_mode(bool)` | `Session::set_approval_mode` with the boolean it replaced: `true` is AUTO, `false` is MANUAL |
| `Session::close()` | `Controller::close_mcp`, then `Controller::close` |

Types:

- `Input` is `#[non_exhaustive]`. `Input::Text(&str)` maps to `Step::Text`
  and `Input::Image(&str, Block)` maps to `Step::Image`.
- `emit` is `otto_core::agent::EventSink`. Events are
  `otto_core::agent::Event`.
- `decide` receives `ApprovalRequest` and returns
  `Future<Output = ApprovalDecision>`. The module re-exports both from
  `app::approval`. A headless host passes a closure that returns `Deny`.
- A new session starts in AUTO approval mode, as in every CLI frontend
  (`approval::Service::mode` defaults to AUTO). AUTO approves a bounded
  authorization without calling `decide` and still asks for a plan review, a
  sandbox escalation or a durable grant. `set_approval_mode(ApprovalMode::Yolo)`
  selects YOLO, in which every request of the session is approved without
  calling `decide`, while a workspace-wide request still reaches it.
  `set_auto_mode(false)` selects MANUAL, in which `decide` is called.
  See the [YOLO mode contract](2026-10-07-yolo-approval-mode.md).
- `prompt` returns `Stop::EndTurn` or `Stop::Cancelled`.
- A second `prompt` on a session whose turn is still running returns the
  existing `PROMPT_ACTIVE` error. Sessions run turns concurrently with each
  other.

Errors are one `#[non_exhaustive] enum Error`. It has these variants:
`AlreadyRunning`, `Config`, `Workspace`, `SessionNotFound`,
`PersistenceDisabled`, `PromptActive`, `Closed`, `Agent(AgentError)` and
`Close(String)`. `Close` carries the redacted message of a failed
`Session::close` or `Otto::close`.
Every message passes through the builder's redaction (`redact_error`), as
serve does.

### Components

`Component` is `#[non_exhaustive]`, so adding a component is not a breaking
change. Every switch puts the runtime into a state the CLI already reaches,
and the switch reaches it through the same code path:

- **Config key.** Where a config key already turns the component off, the
  switch sets that key to `false` on every `File` the runtime resolves. This
  happens after `ManagedPolicy::apply`, at startup and in `load_workspace`
  for each additional workspace. The change exists only in memory. No config
  file is written.
- **Existing fallback.** Where no key exists, the switch takes the branch the
  CLI already takes when a flag is set or a store fails to open. It does not
  print the warning the failure branch prints.

| `Component` | Disabled effect | Existing CLI state reused |
| --- | --- | --- |
| `Bash` | no `bash` tool, no sandbox executor, no sandbox reloader; workspace context skips `git status` | the sandbox reports bash unavailable (`bash_available = false`, checked by `bash_configured()`) |
| `Mcp` | no MCP server process, no MCP tools | `[mcp].enabled = false` |
| `Memory` | memory service not opened (`Service::null(Disabled)`), no memory tools, no memory advisor | `[memory].enabled = false` |
| `Skills` | no skill catalog, no `skill` tool | `[skills].enabled = false` |
| `Plugins` | no plugin tools, no plugin hooks | `[plugins].enabled = false` |
| `Subagents` | no subagent tools (`build_subagents` returns empty) | `[agents].enabled = false` |
| `Reflection` | no reflect tool, no automatic reflection | `[reflection].enabled = false` |
| `Schedule` | `schedule.db` not opened; the schedule tools stay and return "scheduled delivery is unavailable: ~/.otto/schedule.db could not be opened" (`schedule/tools.rs:35`) | `open_schedule` returned `None` (`cli/run.rs:1564`) |
| `SessionPersistence` | no session files; sessions use `SharedSession::memory`; `resume_session` and `list_sessions` return `PersistenceDisabled` | `--no-session` (`Shared.no_session = true`) |
| `Usage` | `usage.db` not opened | usage store failed to open (`cli/run.rs:701`) |
| `TaskHistory` | `tasks.db` not opened | task store failed to open (`cli/run.rs:711`) |

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

### Behavior reference: `otto acp`

The CLI frontends differ from each other in elevation, logging and the
schedule worker. Among them, `otto acp` is the one whose shape matches an
embedded host: sessions created by a program, and approvals answered by the
program. So the embedded runtime takes the `otto acp` value for each of
these:

| Behavior | `otto acp` | Embedded |
| --- | --- | --- |
| sandbox elevation | `elevate = true` (`cli/run.rs:848`) | the same; in MANUAL approval mode, commands outside the sandbox grants reach `decide` |
| approval mode | AUTO for a new session; `_otto/approval_mode/set` switches it (MANUAL, AUTO or YOLO) | AUTO for a new session; `Session::set_approval_mode` switches it |
| logger | `Logger::stderr()` (`cli/run.rs:758`) | `Logger::stderr()`, or `Logger::new(writer)` when `diagnostics` is set |
| startup warnings | written to stderr | written to stderr, or to the `diagnostics` writer |
| session create and load | `Controller::create` and `Controller::open` | the same calls |
| runner build | `build_runner_quiet` (through `Controller::create`) | the same |
| schedule worker | none (`cli/run.rs:1134` starts one only for REPL and TUI) | none |
| session files | `~/.otto` session root, append-only JSONL | the same files; the CLI lists and resumes them, and the reverse also works |

The `diagnostics` writer only changes where the text goes. The text and its
order are the ones `otto acp` writes.

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
  `pub(crate)` phase functions. Each phase is the unchanged code of a
  contiguous run of steps:

  | Phase | Steps |
  | --- | --- |
  | `load` | environment snapshot, redaction boundary, home, layered config |
  | `open_storage` | `Storage::open_home_with` |
  | `configure` | config diagnostics, config environment, redaction merges, ChatGPT credential capture |
  | `canonical_workspace` | canonical `--cwd` |
  | `prepare_workspace` | memory config, workspace marker, session root |
  | `compose` | shell, sandbox settings, MCP, usage/tasks/schedule/skill-check/reflection stores, approvals, `Shared`, `Builder::for_workspace` |
  | `initial_runtime` | the initial `Runtime` |
  | `compose_workspace_sandbox` | sandbox, elevated approval executor, reloader |
  | `open_memory` | memory service, workspace scope, advice audit |

  The CLI interleaves its own steps between the shared ones: the ACP
  listener policy check, the session-operation check, the `@file` prompt,
  `--attach`, `--archive`, `--continue`, frontend selection, resume
  preparation, the cancellation checks and the startup trace marks. A
  single `start` function therefore cannot serve the CLI without moving
  those steps and reordering stderr. The CLI calls the phases with its
  steps in between; `startup::start` calls the same phases in the same
  order with no CLI step, `elevate = true` and no sandbox driver override,
  which is the `otto acp` path.
  - Each phase takes the `stderr` writer and writes warnings when its step
    emits them. The CLI passes its stderr, so its output keeps the same
    bytes in the same order. A test pins that output. One timing changed:
    the `memory/open` startup trace mark is now recorded after the advice
    audit opens instead of before it, because both happen inside
    `open_memory`. The mark's name and text are unchanged.
  - `Disabled` carries the disabled components. The CLI passes
    `Disabled::default()`. `startup::start` applies the config keys after
    `configure`, so the redaction set does not depend on the switches.
  - `startup.rs` also holds `load_workspace` (from `cli/run.rs`).
    `admit_workspace`, `canonicalize_workspace_roots` and `trusted_projects`
    moved from `cli/serve.rs` to `runtime/workspace_admission.rs` in #387
    (cross-workspace delivery); `embed` uses that module.
- **Moved `pub` items keep their old paths.** `cli/sandbox_switch.rs` moves
  to `runtime/sandbox_switch.rs`, and `cli::sandbox_switch` re-exports it.
  `CapturedAuth` and `capture_auth_credentials` move to `auth`, and
  `cli::login` re-exports them. The other moved items are `pub(crate)` or
  `pub(super)` helpers; `cli::run` re-exports the ones other `cli` modules
  call. `cli::run::run`, `cli::run::run_with_backends` and
  `cli::run::Frontend` keep their paths and signatures.
- **`cli/run.rs::run_with_backends`** calls the phases and keeps the
  CLI-only steps and frontend dispatch: workflow, acp, serve, REPL/TUI,
  herdr and the schedule worker. `cli/serve.rs` calls
  `startup::load_workspace` and `workspace_admission::admit_workspace`.
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
other `pub` items in `otto` stay `pub` for the frontends and tests. The
`otto::embed` rustdoc and `docs/development.md` state this.

`otto::embed` changes only by addition, including below 1.0:

- no item is removed or renamed
- no signature changes incompatibly
- `Component`, `Input` and `Error` are `#[non_exhaustive]`
- `Otto`, `OttoBuilder` and `Session` have no public fields
- configuration is set through builder methods, so a new option is a new
  method

When a behavior must change, a new item is added next to the old one, and
the old one keeps working.

## Compatibility and safety

### No breaking change

Phase 1 changes none of the following:

- CLI flags, exit codes, stdout and stderr text and their order
- `otto serve` HTTP API, ACP messages, `otto acp` stderr output
- config keys, config file format, managed policy handling
- session JSONL format, `~/.otto` store schemas and file names
- `pub` Rust items outside `otto::embed`, and the `otto-core` API
- the dependency versions the `otto` binary builds with

The move to `runtime/startup.rs` changes where code lives, not what it does.
Step order, error text and warning text stay the same.

### Other guarantees

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

1. **Default parity with `otto acp`.** For one workspace, config and
   environment, a default `Otto` session and an in-process `otto acp` run
   send the scripted provider equal values for:
   - tool definitions (name, description, schema)
   - model and thinking
   - the system prompt, after the per-run paths are normalized

   The test also compares `otto acp` stderr with the diagnostics writer and
   the file tree under each home. This catches drift between the two.
   Implemented limits: sandbox info and the elevation flag are not compared
   directly (both runs use driver `off`, and both pass `elevate = true`
   through `startup::compose_workspace_sandbox`); subagent tools are empty
   under the scripted provider; Seatbelt behavior is not exercised.
2. **No regression.** The existing CLI, serve and ACP tests pass, and no
   assertion in them changes. Only `use` lines that name a moved non-`pub`
   item may change. One test runs a startup that emits warnings, captures
   stderr, and compares it with a fixture taken before the move.
3. **Session interoperability.** A session created through `Otto` is listed
   and resumed by the CLI composition, and a CLI session is resumed through
   `Otto`.
4. **Each switch.** One test per `Component`:
   - the tool names the table lists are absent
   - the database file it lists is not created under the test home
   - for `Mcp`, a configured stdio server is not spawned
   - for `SessionPersistence`, no `.jsonl` file is written
   - for `Schedule`, `schedule.db` is not created and the schedule tools
     remain
   - for `Plugins`, the test checks that `[plugins].enabled` is false in the
     composed config; no plugin fixture is installed
5. **Disable wins over config.** `[mcp].enabled = true` in config together
   with `disable(Component::Mcp)` gives no MCP tools. The same holds for a
   second workspace opened with `open_workspace`.
6. **Managed policy applies.** Not implemented as a test. `managed_path()`
   is fixed (`/Library/Application Support/Otto/policy.toml` on macOS,
   `/etc/otto/policy.toml` on Linux) and `policy_at` requires root-owned
   ancestors, so an offline test cannot install one. The order holds by
   construction: `config::layered::load_user` and `load` apply the managed
   policy while loading; `Disabled::apply` runs later, in `startup::start`
   after `load` and `configure` and in `startup::load_workspace` after the
   project load, and it only sets keys to `false`.
7. **One instance.** A second `build()` returns `AlreadyRunning`. A `build()`
   after `close()` succeeds.
8. **Prompt round trip.** The session is switched to MANUAL with
   `set_approval_mode(ApprovalMode::Manual)`. A scripted provider calls the `request_approval`
   tool; with driver `off` bash has no approval path, and the
   approval-capable bash definition exists only under Seatbelt.
   - `decide` is called once with that request
   - `Deny` produces the denial tool result
   - the turn ends with `EndTurn`
   - the events arrive at `emit` in order
9. **Diagnostics.** A config that produces a known startup warning writes
   the same text to the `diagnostics` writer that `otto acp` writes to
   stderr.
10. **Boundary.** `runtime_boundary.rs` passes with `embed.rs` present.
11. **Example.** The `otto::embed` module docs contain a `no_run` doc-test of
    the example above, so `cargo test --doc` compiles it.

## Documentation

- `otto::embed` rustdoc is the canonical reference.
- `docs/code-organization.md`: entries for `runtime/startup.rs` and
  `embed.rs`.
- `docs/development.md`:
  - the compatibility surface
  - the additive-only rule for `otto::embed`
  - the rule that `startup.rs` is the only composition root
- `README.md`: a short "Use Otto as a library" section with the git
  dependency line, pinned to a commit with `rev`. The repository has no
  release tags for the Rust implementation yet.
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
- No schedule worker runs in the host process, the same as `otto acp`. In
  the CLI the worker runs only for the REPL and TUI frontends
  (`cli/run.rs:1134`). Schedule tools still create entries, and
  an `otto` REPL, TUI or `otto serve` process whose worker scope matches
  runs them.
- No offline test installs a managed policy; see [Testing](#testing) item 6.
- The API is Rust only and needs a Tokio runtime.
- Phase 1 is a git dependency. Builds of the host compile all of `otto`,
  including the TUI and server modules.

## Phase 2: publication on crates.io

Phase 2 is a separate spec and PR. It is listed here so that phase 1 does
not block it. The no-breaking-change rule applies to it too:

- the `otto` binary keeps the dependency versions it builds with
- the user manual that the `otto` help tool embeds stays byte-identical

These items must be resolved:

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
   - caret ranges plus the committed `Cargo.lock`. The lock file keeps the
     binary's resolved versions, but the documented rule changes.
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
3. **No breaking change, consistent behavior.** Existing behavior and
   interfaces do not change. The embedded runtime behaves the same as the
   CLI, and `otto::embed` changes only by addition.

Proposed in this spec, accepted by approving it:

4. `otto acp` is the [behavior reference](#behavior-reference-otto-acp).
   This covers elevation, logging, warnings, session paths and the absence
   of a schedule worker.
5. The component list and the way each switch is applied, as in the
   [component table](#components). Every disabled state reuses an existing
   CLI state. File-tool and web-tool switches are left out for that reason.
6. Switches only disable. There is no code switch that runs bash unsandboxed.
7. One `Otto` per process.
8. Config source is `Default` or `Path`. There is no inline config and no
   `api_key` setter.
9. crates.io publication is phase 2, under its own spec.
