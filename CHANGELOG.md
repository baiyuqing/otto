# Changelog

Notable resulting changes, newest first. This is a curated summary, not a
commit ledger; Git and release tags retain the full history. See the
[user manual](docs/user-manual.md) for current behavior and the
[changelog rules](docs/development/workflow.md#changelog) for maintenance.

## Unreleased

### Security

- Custom provider endpoints now fail closed until their exact origin is explicitly
  trusted in user or managed configuration; existing custom endpoints require this
  review after upgrading. Plain HTTP needs a separate acknowledgement. Provider
  requests reject redirects and metadata/link-local destinations, validate DNS
  answers, and ignore environment/system proxies. Restart all Otto processes after
  upgrading. Trust grants are not a certification that a service is benign; review
  the [endpoint trust and recovery guidance](docs/manual/getting-started.md).
  Embedders must use validated endpoint capabilities for custom origins; injected
  HTTP clients are no longer accepted.
- SQLite operations reject stacked statements before touching the live connection,
  while values remain parameter-bound. Legacy storage imports and approval backups
  reconstruct recognized canonical schemas rather than executing source schema SQL.
  Noncanonical legacy databases are rejected without publishing an imported
  destination; see [storage migration](docs/manual/memory-and-extensions.md).

### Server

- `otto serve` can start before a model is configured so first-run clients can
  use the authenticated provider/model configuration API. Session creation
  returns actionable `503 configuration_required` until setup is saved and
  the server is restarted. Other frontends still require a model at startup.

### Extensions

- Installed plugin hooks and custom tools can use built-in Lua 5.4 handlers with
  no external interpreter. Lua is restricted to pure computation with fresh VMs,
  bounded resources and cooperative cancellation/revocation; package trust and
  tool approvals remain required. Command handlers keep their existing behavior.
  Remove Lua packages before downgrading to older Otto binaries. See
  [Lua handlers](docs/manual/memory-and-extensions.md#lua-handlers).

### Memory

- Sessions now record which memory records each turn and each compaction
  checkpoint saw, by ID and revision only. Each turn writes an
  `otto.memory_recall` entry, and checkpoints carry `memoryRecords` and
  `memoryCoverage`. Recalled record text is still not persisted. Turns and
  checkpoints written before this change read as unknown coverage. Older Otto
  binaries read sessions with the new entries unchanged (#591).
- The file store keeps a revision history of each record in
  `history/<id>/<revision>.md`. Inspect and restore it with
  `otto memory history|diff|restore` and `/memory history|diff|restore`; a
  restore writes a new revision and never lowers the revision number. Forget now
  also removes the history and the text of pending candidates that target the
  record, and prints each remaining copy as removed, suppressed, present or
  unverified, including the advice audit log and session files. A record file
  deleted by hand while its history exists is tombstoned the next time the store
  opens, and its history is removed (#591).
- Memory records now have a lifecycle state (`active`, `needs_review`,
  `superseded`, `retracted`) and the times `verified_at` and `review_at`. Only
  active records whose review deadline has not passed are recalled. Set them with
  `otto memory state|verify` and `/memory state|verify`, set a deadline with
  `/remember --review-in AGE`, and filter `list` by state, kind, label, update
  and verification time, `--unverified` and `--review-due`. Record files without
  the new fields read as active (#591).
- Before every provider request, Otto re-checks the state of the memory records
  the conversation refers to: records recalled at the start of the turn that
  were since forgotten or retired leave the request, and a fixed-template
  `<memory_retraction .../>` notice (id, revisions and reason only, never text)
  tells the model which earlier records are superseded, retracted, under review,
  expired, updated or forgotten. Notices come from the references recorded in
  the session and its checkpoints, so they survive a resume; turns without
  references and plugin stores that no longer return a record produce none. See
  [retraction notices](docs/manual/memory-and-extensions.md#retraction-notices)
  (#591).
- A compaction summary written from turns that used a since-retired memory
  record is marked in every request (`<memory_summary_affected .../>`) with a
  one-time warning per turn; a checkpoint without references or a
  provider-managed compaction is marked as unknown coverage. `/compact` and
  automatic compaction rebuild the summary with the retracted records listed
  for the summarizer and record them in the checkpoint's `memoryRetracted`, so
  the mark stops. The rebuild uses the previous summary and the messages since
  the checkpoint; `/new` is the complete recovery path (#591).
- Memory candidates can now carry the actions `retract`, `supersede`, `correct`
  and `needs_review` besides `create`, `update` and `forget`; accepting one sets
  the target record's state or creates the replacement record. `/memory propose`
  with an id and evidence text asks the model for a validated, evidence-backed
  candidate for one record and queues it for `/memory review`; the exchange is
  stored in the advice audit log. See
  [local memory use](docs/manual/memory-and-extensions.md#local-memory-use)
  (#591).

### Sessions

- Permanently delete sessions through the TUI, `/delete`, `otto delete <SESSION>`,
  or the separately confirmed HTTP endpoint; the CLI accepts an ID or unique
  exact name, with IDs taking precedence and `--force` limited to IDs. Unlike
  archive, deletion is irreversible. After upgrading, restart Otto processes
  before using the new session operations. See [session lifecycle](docs/manual/interactive-use.md#session-lifecycle)
  and [the HTTP API](docs/manual/servers-and-connectors.md#http-api).

- New session content and pending-input mailbox files receive local credential
  detection by default, covering common cloud keys, OAuth secrets, connection
  credentials and selected wallet formats. Tool execution keeps its original
  arguments; masked arguments are not automatically replayed after a crash.
  Existing logs and the approval-content exception are preserved. See
  [session credential redaction](docs/manual/interactive-use.md#session-credential-redaction)
  for coverage and limits.

### Agents

- Observe sub-agent progress in a read-only TUI page with stacked task panes;
  `F6` toggles it and `Esc` returns by default; `[ui.observer]` customizes the
  toggle/open/back bindings without occupying macOS Control-arrow shortcuts.
  While children are queued/running, one main-footer hint advertises the configured
  entry key (`Press F6 to view sub-agent observer` by default); status notices stay
  first. The main draft is preserved. Local and attached sessions share bounded live
  snapshots, with stored child history after restart. Upgrade and restart both
  Otto and `otto serve` before using the attached observer. See
  [the observer guide](docs/manual/interactive-use.md#sub-agent-observer).

- Delegate tasks to installed Codex, Claude and Pi ACP adapters using the
  existing agent task controls. External agents use their own sandbox and
  permissions.

### Sandbox

- A sandboxed `bash` command reads every host path except a denylist, instead of
  only an allowlist of system directories, `PATH` directories and `read_paths`.
  The built-in denylist covers credential, shell-history and personal-data
  locations under the home directory and the private sandbox state roots
  `~/Library/Caches/otto-sandbox-*`. Paths not on the list are readable, so a
  credential at an unlisted path can be read and, with `network = "allow"`, sent
  off the host. Writes are unchanged. See
  [the read denylist](docs/manual/safety-and-tools.md#a-sandboxed-command-reads-every-path-except-the-read-denylist).
- `[sandbox] deny_read_paths` adds absolute or `~/...` paths to the denylist and
  takes effect on `/sandbox reload`. An entry that equals or contains a writable
  root is rejected and the sandbox does not start.
- `[sandbox] read_paths` now lists exceptions inside the denylist. An entry that
  is not at or below a denied path is inert. `/sandbox` prints `Sandbox
  read_paths <path>: not below any denied path; remove it from read_paths` for
  it, and `Sandbox deny_read_paths <path>: contains a writable root; not
  applied` for a built-in entry that contains a writable root. Existing
  configurations load unchanged; review `read_paths` entries that `/sandbox`
  reports.
- The sandboxed command's `HOME` is your real home directory and the private
  per-session `home` directory is removed. `ZDOTDIR` points to the private
  `tmp` directory, so `zsh -c` does not source `~/.zshenv`, `~/.zprofile` or
  `~/.zshrc`. `CARGO_HOME` points to a private per-session cache directory and
  `RUSTUP_HOME` is not set. The cargo, go, npm, pip and uv caches download again
  in every session, and cargo does not read `~/.cargo/config.toml`. A write
  under `~` outside the writable set fails; use `TMPDIR` for scratch files.
- `sandbox_read_path` and `/sandbox allow` accept only a path the denylist
  covers; for another path the `bash` tool returns `path is already readable;
  run the command without sandbox_read_path` and `/sandbox allow` reports `path
  is already readable; no read_paths entry is needed`. A `sandbox_denied:`
  section with a `file-read` denial ends with a hint to request an exception
  with `sandbox_read_path` or an elevated run with `sandbox_permissions`. The
  `bash` and `sandbox_read_path` descriptions describe the denylist.
- The HTTP `Sandbox` object returned by `POST /v1/sandbox/reload` gains the optional
  string arrays `inert_read_paths` and `skipped_deny_paths`, present only when
  non-empty.
- `otto sandbox setup`: the GitHub CLI recipe adds the `gh` configuration
  directory to `read_paths` as an exception to the denylist. It no longer adds
  `GH_CONFIG_DIR` to `allow_env` and no longer prints a launch command.

### Configuration

- LLM access is configured as providers and models instead of
  `[profiles.<name>]`. `[providers.<name>]` holds `type`, `base_url`,
  `api_key_env`, `api_version` and `workspace_id_env`, and one line per model,
  `models."<model_id>" = { thinking = ..., context_window = ..., compaction_window = ... }`
  (the table form `[providers.<name>.models."<model_id>"]` is also accepted).
  `claude`, `openai-compatible` and `chatgpt` are built-in providers, so a
  ChatGPT model needs only `models."<model_id>" = {}` under
  `[providers.chatgpt]`. See
  [provider and model configuration](docs/manual/getting-started.md#providers-and-models).
- `otto setup`, `otto config set` and `POST /v1/config/changes` write a new
  model as a `models."<model_id>"` line inside `[providers.<name>]`, creating
  that table when the file does not hold it, and edit or remove an existing
  model in either form in place; a provider whose models are all written as
  `[providers.<name>.models."<model_id>"]` tables receives a new model in that
  form. `otto config set` accepts a quoted model ID in the key
  (`providers.chatgpt.models."gpt-5.6-luna".thinking`). The legacy-profile
  diagnostic prints the one-line form.
- [examples/config.example.toml](examples/config.example.toml) is a commented
  sample user configuration in the new format; an otto-core test parses it and
  resolves each of its models.
- A model is selected as `<provider>/<model_id>` by `default_model`, `--model`,
  `OTTO_MODEL`, `/model` and `approval.reviewer_model`. `--model` requires this
  form; a bare model ID is an error unless `--provider` is also given.
  Projects may set `default_model` and the `thinking`, `context_window` and
  `compaction_window` of model entries, not providers.
- HTTP: `GET /v1/config/providers`, `GET /v1/config/providers/{name}` and
  `GET /v1/config/models?provider=<name>` replace `/v1/config/profiles[/{name}]`.
  Configuration writes go through `POST /v1/config/changes` with the kinds
  `set_default_model`, `set_provider`, `set_provider_field`, `remove_provider`,
  `set_model`, `set_model_field` and `remove_model`. `POST /v1/sessions/{id}/model`
  with `{"model":"<selector>"}` replaces `POST /v1/sessions/{id}/profile`. The
  old routes are removed and return `404`, without aliases. Response objects
  drop `profile`, add `provider_type`, and `provider` is
  the provider name; `profiles` in `GET /v1/info` is now `models`, a list of
  selectors.
- Existing `[profiles.*]`, `default_profile`, `approval.reviewer_profile`,
  `--profile`, `OTTO_PROFILE` and `--provider` with a bare `--model` still
  work. Each legacy file produces a diagnostic with the equivalent new TOML;
  Otto does not rewrite it. A managed policy that forces `default_profile` or
  `profiles.*` fails to load and names the new keys. Existing sessions, usage
  records and workflows load unchanged.

### Plugins

- Hook protocol v1 payloads carry `session.history_path` (the session file
  path, empty while a new session has no file yet) and a top-level `headless`
  flag (true for `otto --prompt` runs), and every event accepts a `notice`
  response field that Otto shows to the user as a notification and never to
  the model. Package hook and custom-tool commands receive `PLUGIN_ROOT` and
  `PLUGIN_DATA` in their environment. See
  [hook protocol v1](docs/manual/memory-and-extensions.md#hook-protocol-v1).
- [examples/plugins/tokeneconomizer](examples/plugins/tokeneconomizer/README.md)
  runs TokenEconomizer's `te-hook --host pi` from Otto's hooks: micro-skill
  injection at session start, the loop guard after each tool call and the
  learning fold at session end. Its state root defaults to
  `<workspace>/.otto/tokeneconomizer`; the TokenEconomizer checkout and
  `~/.otto/sessions` must be listed in `[sandbox] read_paths`.
- Packages with a `.claude-plugin/plugin.json` run the `command` hooks of
  their `hooks/hooks.json` and manifest `hooks` field natively, with Claude
  Code event names, stdin payloads, exit-code and JSON response handling,
  matchers and `${CLAUDE_PLUGIN_ROOT}` substitution, under Otto's trust,
  sandbox and size limits. `otto plugin inspect` lists them under
  `claude_hooks`. A `claude` package trusted before this change runs its hooks
  after the upgrade without a new trust step. Hooks in Claude settings files
  and in `codex` packages are not read. See
  [Claude Code hook format](docs/manual/memory-and-extensions.md#claude-code-hook-format).

### Fixes

- A model or reasoning-effort switch no longer cancels the session's
  running sub-agents, overwrites its pending notifications, restarts its stdio
  MCP servers or stops `otto serve` from waking the session on sub-agent
  notifications. The sub-agent registry, inbox, timers, MCP servers and
  workflow delivery worker belong to the session and are stopped by `/new`,
  `/load`, session close and process exit. Sub-agent bash tools now use the
  owning session id as the approval key instead of one key shared by every
  session of a workspace. `[agents].max_parallel` caps the running sub-agents
  of the session, including those started before the switch.

### Changes

- TLS uses rustls with the `ring` crypto provider instead of `aws-lc-rs`, for
  provider and OAuth requests, `web_fetch`, HTTP MCP servers and the
  `otto serve --listen` TLS listener. TLS 1.3 key exchange is X25519 or P-256;
  the hybrid post-quantum exchange that `aws-lc-rs` offered is no longer
  negotiated. Cipher suites, certificate verification (platform verifier) and
  certificate formats are unchanged.
- Bash overflow logs, research scratch files and sub-agent reports are written
  under `.otto/tool-output/<session-id>/` and `.otto/reports/<session-id>/`
  instead of directly under `.otto/tool-output/` and `.otto/reports/`.

### Providers

- Added a first-party Claude Messages API provider with environment-only API-key
  authentication, streaming text and local tool calls, model discovery, typed
  context-overflow recovery, bounded retries and diagnostic redaction. Claude
  providers work in terminal, server, ACP and `otto-connect` sessions; the key
  remains in the Otto runtime and is never stored by the connector.

### Development

- The Cargo workspace has five crates instead of two: `otto-core`, `otto`,
  `otto-server` (the former `otto::server`), `otto-acp` (the former
  `otto::acp`) and `otto-cli` (the former `main.rs` and `otto::cli` command
  modules, which now owns the `otto` binary). `otto` stays the library with
  the `embed` API; `cli::{login, repl, repl_commands}` stay in it because the
  TUI uses them. Dependencies are one-way, `otto-core` <- `otto` <-
  `otto-server` <- `otto-acp` <- `otto-cli`, so a change in a frontend crate
  no longer rebuilds the `otto` library test binary. The process integration
  tests moved to `crates/otto-cli/tests/`. `otto::fixtures` gained
  `embed_harness`, `plugin_package` and `subagent_doubles`, and test-only
  items that dependent crates use are public under
  `cfg(any(test, feature = "test-support"))`. Build with
  `cargo build -p otto-cli` and run moved tests with
  `cargo test -p otto-cli --test <name>`; `cargo test -p otto --test tui_pty`
  is no longer valid.
- The test scaffolding shared by the crate's tests is `otto::fixtures`, compiled
  under `cfg(test)` or the new `test-support` feature (which implies
  `fault-injection`); it holds the former `cli::fixture_tests` and the scripted
  provider server from the integration-test harness, so the tests of dependent
  crates can use both through a dev-dependency. First step of the crate split.
- `otto eval view-tree` evaluates a summary-tree context view on an existing
  session: it builds the tree with the compressor model, generates and
  validates recall probes, answers them with zoom over the view and with the
  current context, and reports correctness and a computed cache cost. See
  [testing](docs/development/testing.md#summary-tree-view-evaluation).

### Observability

- `GET /metrics` now reports the Otto server process's resident and virtual
  memory, cumulative CPU time, thread count and open file-descriptor count on
  macOS and Linux. An unavailable operating-system sample is omitted without
  failing the metrics scrape.

### Security

- Updated `rustls-webpki` to 0.103.16 to reject certificate paths through the
  distrusted emSign intermediate public keys described by CVE-2026-111943.
  Source installations must rebuild Otto to receive the fix.
- ChatGPT login, logout and token refresh now serialize credential changes and
  conditionally commit refreshed tokens, so an in-flight refresh cannot restore
  a signed-out account or overwrite a newer login. A request whose current token
  is rejected refreshes it and retries once; policy-restricted capacity failures
  remain distinct from retryable server overloads.
- Every provider request now receives always-on, local PII filtering in addition
  to credential filtering. High-confidence email addresses, international phone
  numbers, checksum-valid payment cards and Chinese resident identity cards are
  replaced with stable typed placeholders in free text and JSON values; PII in
  structural or opaque fields refuses the request. There is no PII configuration
  or remote classification.
- The Seatbelt sandbox no longer admits a directory as a writable worktree
  only because `git worktree list` names it. A sandboxed command could write
  `.git/worktrees/<id>/gitdir` with any path, and the next `/sandbox reload`,
  approval reload or restart made that path writable. Otto now admits a listed
  path only when its own `.git` entry links back to the workspace's common Git
  directory, and skips `bare` and `prunable` records.
- Below `.git/modules` (submodule Git directories) the files `config`,
  `config.worktree` and `commondir` and every `hooks` directory are now
  read-only in the sandbox, as is `config.worktree` below `.git/worktrees`.
  `git submodule update` fails in the sandbox when it has to clone a submodule
  into `.git/modules`; run it outside the sandbox. The `write` and `edit`
  tools additionally refuse, at any depth, a `.git` entry and, below any
  `.git` component, the files `config`, `config.worktree`, `commondir` and
  `gitdir` and every `hooks` directory. See
  [git metadata is read-only](docs/manual/safety-and-tools.md#git-metadata-is-read-only-in-the-seatbelt-sandbox).

### Development

- Clean dev builds are shorter. Measured with `cargo build --timings` on macOS
  arm64 (Rust 1.99.1, 10 cores, 2026-11-01): a clean `cargo build` went from
  65.0 s to 52.2 s, and a rebuild after a one-line edit in `crates/otto` from
  8.8 s to 6.1 s. Two changes: the `ring` crypto provider replaces
  `aws-lc-rs`, whose build script compiled C for 16.7 s on the critical path
  (ring's takes 2.8 s), and the dev profile sets `debug = "line-tables-only"`.
  Backtraces and panic messages keep file and line; a debugger no longer shows
  local variables or types in dev and test builds. A developer who needs them
  sets `[profile.dev] debug = 2` in `~/.cargo/config.toml`, which overrides
  the manifest. Every reqwest client is created through
  `otto::tls_provider::http_client_builder()` or `http_client()`, which
  install the provider; a `reqwest::Client::builder()` call elsewhere panics
  at `build()` with "No rustls crypto provider is configured".
- `make install` now includes a repository-aware `rust-best-practices` skill,
  adapted from BeautifulRust with source attribution. It applies idiomatic Rust
  guidance while keeping Otto's scoped `AGENTS.md`, architecture, exact
  dependency pins and focused local verification authoritative.
- Release builds of `otto` now abort on panic. A panic in any task, including
  one inside `otto serve`, ends the whole process with SIGABRT (exit status
  134) instead of ending only that task; before exiting, Otto kills the
  sandboxed bash and MCP stdio process groups it started and restores the
  terminal. Run `otto serve` under a supervisor that restarts it, such as
  `scripts/otto-tcp.sh run`. Debug and test builds still unwind.
- The release profile now builds `otto` with fat LTO, one codegen unit,
  `opt-level = "s"` and `panic = "abort"`, and strips symbols. On macOS arm64
  with Rust 1.99.0 the `make build` binary shrinks from 65,633,504 to
  25,443,120 bytes, and a clean release build takes about 90 s longer (170 s
  to about 260 s on an Apple M4). Debug and test builds are unchanged.
  `RUST_BACKTRACE` output no longer shows function names; panic messages still
  include file and line. Rerun `make install` to pick up the smaller binary.

### Performance

- Local memory automatically moves accepted/rejected proposals into
  `candidates/archive/`, including existing decided files on startup, so normal
  memory reads no longer scan that history. Receipts remain available for
  exact-ID lookup and explicit history lists; older Otto versions cannot find
  archived receipts. Files are retained, not deleted.

- The terminal UI now keeps the rendered rows of every transcript entry
  between frames and re-renders only the entries that changed, instead of
  rendering the whole transcript again on every tick. At 180x50 with a
  300-turn session loaded, idle CPU drops from 15.3% to 2.1% (2.76 s to
  0.37 s over 18 s) and resident memory rises from 63 MB to 65 MB; an empty
  transcript stays at 1.6%.
- Scanning each provider request for secrets before it leaves the process
  now uses one prebuilt substring finder per secret value and a heuristic scan
  that stops at the first finding, instead of one `str::replace` per secret
  value and text field and PEM boundary strings rebuilt on every call.
  Redaction output is unchanged. With a loopback mock provider and 1000 tool
  steps over a context growing to 2002 messages, the pause between a model
  response and the next request drops from a median of 25.9 ms (47.7 ms over
  the last ten steps) to 2.1 ms (3.6 ms), and the turn's wall time from
  26.4 s to 2.6 s.

### Features

- `otto serve` can now listen on a non-loopback TCP address for a client on
  another machine. This needs `[server].tls_cert` and `[server].tls_key` (PEM
  files; the listener terminates TLS and the startup line uses `https://`)
  and `OTTO_SERVER_TOKEN` (32 to 4000 printable ASCII bytes), which replaces
  the random per-process token and is never printed; without both, a
  non-loopback `--listen` host still fails at startup. On such a listener
  every path except `GET /healthz` needs the token, `/metrics` included, and
  `http_request` log lines carry the client address as `peer`. Loopback
  listeners behave as before, and `OTTO_SERVER_TOKEN` alone makes a loopback
  token stable across restarts. See
  [Listener](docs/manual/servers-and-connectors.md#listener).
- In the terminal, `Esc` while guidance is queued for the running turn now
  delivers it at once instead of cancelling the turn: the running model or tool
  call is stopped, the guidance is appended, and the same turn continues. `Esc`
  with nothing queued and `Ctrl+C` still cancel the turn. `otto serve` exposes
  it as `POST /v1/sessions/{id}/tasks/steer/interrupt` and streams a
  `step_interrupted` event; see
  [Interactive use](docs/manual/interactive-use.md) and the
  [HTTP API](docs/manual/servers-and-connectors.md#http-api). Reinstall Otto
  and restart terminal sessions.

### Fixes

- With at least one plugin installed, a new session no longer creates its
  session file before the first prompt. The `otto.plugins` record is now
  written together with the first real entry. Previously every new session
  was written to disk at start with only this record, so it appeared in the
  `/archive` panel, `/resume` and `--continue`; archiving the current session
  then produced another such session each time, and the list never emptied.

- Local and attached TUI approval notices now show labeled states, reasons,
  feedback and literal request details. Serialized JSON detail values display
  readable newlines/indentation without decoding literal command backslashes or
  allowing terminal control sequences. Updates replace one notice per approval;
  same-session refreshes retain live notices. Stored history and approval behavior
  are unchanged; structured notices are not reconstructed after restart/resume.
  Reinstall Otto and restart the terminal TUI to use the fix; no configuration
  changes are needed.

### Breaking changes

- Approvals, schedules, workflows, tasks, usage, skill checks and memory advice
  are stored through SQLite (bundled by `rusqlite` 0.41.0) instead of the
  turso 0.8.3 engine. Existing databases under `~/.otto/` open without
  conversion. **Stop every Otto process, including `otto serve` and connectors,
  before starting the new binary:** turso processes coordinate through a
  `<database>-tshm` file and SQLite processes through `<database>-shm`, so an
  old and a new process must not use one database at the same time. The memory
  database of 0.4.8 (`~/.otto/memory/memory.db`) is no longer imported into the
  file store, because SQLite cannot read its turso full-text index; to keep
  those records, run a build of commit 96646600 once with all other Otto
  processes stopped, then upgrade. `[memory.turso].path` is accepted and
  ignored. On an Apple M4, a clean release build takes 139.8 s instead of
  248.4 s and the release binary shrinks from 25,459,648 to 16,604,928 bytes.

- Remove the terminal `/steer` command, including in attached sessions.
  Ordinary text typed while a turn runs still steers that turn; when no turn is
  active, such text starts a new turn. Chat, ACP and HTTP steering are
  unchanged. Reinstall Otto and restart terminal sessions.

- Remove the terminal `/plan` command, including in attached sessions. The
  status line still shows `Plan n/m · <step in progress>`; the `update_plan`
  tool, ACP plan updates and HTTP plan output are unchanged. Reinstall Otto and
  restart terminal sessions.

- Remove the terminal `/image` and `/trace` commands, pending terminal image
  attachments and the trace timeline panel, including in attached sessions.
  Terminal prompts are text-only; image messages through supported APIs and
  stored tracing, logs and trace APIs remain available. Reinstall Otto and
  restart terminal sessions.

- Remove reflection completely: `/reflect`, the model-callable tool, automatic
  triggers, configuration, run reports and generated-skill management are no
  longer available. No compatibility or data migration is provided. Remove
  `[reflection]` from existing configuration, reinstall Otto and restart Otto
  and connector processes. Reflection data is disposable, not retained by the
  feature; remove old `~/.otto/reflection/` and `~/.otto/reflection.db*` artifacts
  when upgrading. Ordinary memory, hand-written skills and compaction remain.

### Usage and compatibility

- Slash commands are listed in workflow-stage groups: help, setup, during a
  task, approvals and sandbox, memory, tasks, sessions, exit. TUI `/help`
  prints a heading per group; TUI completion, REPL `/help`, ACP
  `available_commands` and the chat command menu use the same group order.
  Command names, arguments and behavior are unchanged.

- `/permissions` is the single approval command. `/permissions
  manual|auto|yolo` applies the presets that `/auto` applied, and
  `/permissions` without arguments also shows the approval mode. `/auto`
  remains an unlisted alias with the same arguments, including `on`/`off`; it
  is removed from `/help`, TUI completion, ACP `available_commands` and the
  chat command menu. Chat clients reject `/permissions yolo`. HTTP and ACP
  approval APIs are unchanged. See
  [approval modes](docs/manual/safety-and-tools.md).

- A plugin package can be the memory source. A package declares a `memory`
  command in `extensions.dev.otto`, and `[memory] backend = "plugin:<package>"`
  in user or managed config selects it. Recall, `memory_search`, `remember`,
  `forget`, `/remember` and turn observation then go to the plugin, and Otto
  opens no file store. `[memory] enabled = false` now turns off memory for
  either source. Downgrade warning: after a package with a `memory` component
  is installed, older Otto binaries reject the plugin registry with "invalid
  plugin registry"; run `otto plugin remove <package>` with the new binary
  before downgrading. See
  [plugin memory source](docs/manual/memory-and-extensions.md#plugin-memory-source).
- `[memory] backend = "remote"` and `[memory.remote]` now fail startup with an
  error naming the otto-mem plugin. Builds from `main` after
  [#493](https://github.com/baiyuqing/otto/pull/493) must move to the
  [otto-mem plugin](https://github.com/baiyuqing/otto-mem) and
  `[plugins.bindings.otto-mem]`. Rust embedders:
  `otto_core::memory::RecallItem` no longer has a `remote` field,
  `RemoteProjection` and `MemoryView` are removed, so a `MemoryService`
  implementation must drop `remote: None`. The memory tools no longer accept
  `organization_id`.
- Plugin hooks receive turn content and package credentials:
  - `turn_end` hook data includes `turn.messages` with the user, assistant,
    tool-call and tool-result blocks appended during the run. When the input
    would exceed 1 MiB it includes `turn_omitted: "size"` instead.
  - A `[plugins.bindings.<package>]` key binds `env` for that package's hook
    and tool commands only. Bash and other packages do not receive these
    variables, and their values are redacted.

  Existing hooks and `plugin:server` MCP bindings are unchanged. See
  [hooks and custom tools](docs/manual/memory-and-extensions.md#hooks-and-custom-tools).

- Parent conversations can discover, inspect, start and check durable workflows
  through four native tools, with conditional execution-selection guidance and
  six prompt examples. Shared workspace ownership, receipt-backed starts and
  saved-session lifecycle feedback preserve conservative recovery and human
  gates. Rebuild with `make install` and restart Otto/connector processes;
  existing workflow definitions are reused and a fresh setup example is in
  the [automation guide](docs/manual/automation.md#choosing-workflows-from-conversation).
  Internal child/workflow recursion is denied; authenticated external API
  callers retain their existing authority. Structured workflow scheduling is
  not added.

- The TUI footer's token totals and cached share now count sub-agent tasks. A
  finished task's tokens travel in its completion notification, which the
  session history stores in an Otto record detail, so the totals also survive
  resume, `--attach`, forks and `GET /v1/session`; older sessions still decode
  unchanged. The cached share stays hidden while any report, including a
  sub-agent's, lacked the cache breakdown. Rebuild and restart; no configuration
  or history migration is needed.

- Compact TUI tool records show live/final locally observed execution time and
  distinguish cancellation, while preserving expandable raw details and full
  operation/provider IDs. Thinking and total-turn durations are no longer shown.
  Unknown history/attach timing is omitted rather than estimated. Enabled by
  default after rebuild/restart; no configuration or migration is required.

- Workflow runs reach a durable outcome instead of staying `running`: a failed
  step settles its run as `failed`, recovery settles every unfinished run, and
  `otto workflow resume --retry` accepts `failed` steps and re-admits the steps
  that failure canceled. Rejecting a gate stops the run's in-flight attempts.
  `GET /v1/workflows` is paginated (`limit`, `before`, `next_before`) and reports
  `400`, `404`, `409`, or `503` by cause instead of `409` for every failure.
  ([#521](https://github.com/baiyuqing/otto/pull/521))

- Delivered queued input remains a visible user message in server/attached TUI
  streams after the queued banner clears. Rebuild and restart `otto serve`, then
  reconnect clients; no configuration or history migration is needed.
  ([#519](https://github.com/baiyuqing/otto/pull/519))

- Memory now defaults to editable files in `~/.otto/memory/`, not a Turso
  database. Recall injects standing preferences and bounded in-scope records;
  search uses case-insensitive substrings. Invalid files are reported by
  `otto memory status`. Memory databases of earlier releases are not imported
  (see the SQLite entry under Breaking changes); remove
  `[storage.routes].memory` from configuration. Memory is no longer a storage
  domain and is not moved by `otto storage transfer`. ([#491](https://github.com/baiyuqing/otto/pull/491))
- Approval content is no longer secret-scanned. It may be displayed and
  persisted: **do not include actual secrets**. Policy, permissions, human
  checkpoints, model redaction and non-approval protections are unchanged.
  Rebuild/upgrade and restart normally; no configuration change is needed.
- Shell approval rules inspect every command in a list or pipeline. Confined
  execution and permission to change external state are separate decisions;
  sandbox exclusions do not bypass approval policy.
- Sub-agent model selection discovers the active provider's catalog on demand,
  falling back to the session model when discovery fails or suitability is
  unknown. Explicit choices and named-agent defaults take precedence;
  `subagent_models` remains an optional deprecated override.
- Newly initialized workspaces use readable path-derived memory scope IDs.
  Existing identity markers retain their adopted IDs; preserve the marker
  when moving a workspace to keep its identity.
- Session resume preserves the current profile/model/reasoning. Slash commands
  require `/` as the first character; `/clear` starts a fresh session.
  `/task` manages current-session child tasks; `/inputs` is removed. Use ordinary
  text for current-turn guidance and `Ctrl+U` to recall undelivered guidance.

### Features and reliability

- OpenAI-compatible requests whose stream fails with a transient transport
  error or a model output idle timeout after partial output are now retried
  within the same four-attempt and deadline budget, as ChatGPT requests
  already are. The partial output is discarded and never runs tools; the
  `provider_retry` event carries `interrupted_output: true`. The repeated
  generation is billed again by the endpoint. Stream codec errors and HTTP
  statuses after output still end the turn.

- OpenAI-compatible requests now send `prompt_cache_key` set to the session id
  (a sub-agent uses its own transcript id), as ChatGPT requests already do.
  An endpoint that answers HTTP 400 or 422 naming `prompt_cache_key` gets the
  request resent once without the field, and later requests from the same
  provider client omit it.

- OpenAI-compatible requests that receive HTTP 429 or 503 before any output
  are retried within the existing four-attempt and deadline budget, waiting
  1/2/4 seconds or a numeric `Retry-After` capped at 30 seconds. HTTP 500, 502,
  504 and other statuses still end the turn after one request. Retries appear
  as `retry A/M after HTTP 429` (or `HTTP 503`) and as `provider_retry` events.

- Keep queued guidance visible in local and attached terminals, including text
  admitted by other clients, and retain its body through delivery. Sending text
  resumes follow-bottom; external messages preserve historical reading position
  with a `Ctrl+End` jump-to-bottom hint above the composer. Rebuild or reinstall
  Otto and restart terminal sessions to use the updated display.

- Show all pending TUI inputs in the main conversation, not only the newest
  composer banner. Pending rows remain transient, become a single delivered
  message, and disappear on withdrawal. Reload active guidance on session
  switches and attached reconnects; reject stale queue reads from earlier
  sessions or turns. Visible-buffer and reconnect tests cover this lifecycle.
- Keep TUI loading animation on its frame cadence while sub-agents run even
  when the main turn is idle, and skip overdue animation ticks instead of
  replaying them in a burst. Approval polling retains its existing cadence.

- The TUI adds compact tool summaries (`Ctrl+O` for details), inline edit diffs
  and searchable skill management. Queue banners keep pending guidance visible
  without replacing the conversation.
- HTTP session creation and fork accept `request_id` for retries after a lost
  response, including after restart. Conflicting fork arguments return
  `409 request_id_conflict`.
- Interrupted tool recovery automatically replays only genuinely read-only
  calls. Effectful or uncertain calls require inspection before a new invocation;
  custom/MCP tools do not gain retry safety from a built-in's name.
- Provider egress checks guard secrets; private rotating execution diagnostics
  exclude request/response bodies. Provider no-output timeouts and repeated
  failed-cycle detection stop stalled turns with actionable failures.
- ChatGPT model catalogs use a compatible Codex version and a private three-day
  cache scoped to endpoint/account/version; rejected model requests invalidate
  it. ChatGPT compaction handles opaque provider context without exposing it as
  model-visible text or undercounting it.
- ChatGPT token refresh is serialized across runners and Otto processes by a
  lock on `~/.otto/auth/chatgpt.json.lock`. Under the lock the credential file
  is re-read: a valid token another runner saved is reused, and an expired one
  is refreshed with the file's latest refresh token. A refresh that completed
  is saved even when the call was cancelled afterwards, and a file holding
  another account's login is not overwritten.
- A stdio MCP call whose request is blocked writing to a server that stopped
  reading stdin now times out at `call_timeout` and stops the server. Before,
  the call and the server shutdown waited indefinitely.
- Concurrency fixes:
  - A workflow attempt that stops early cancels its sub-agent, and a
    sub-agent that finishes while being cancelled keeps its result.
  - A parent `agent_send` message that arrives during a sub-agent's last
    provider call is delivered before the sub-agent finishes.
  - The idle session sweeper no longer closes a session that started a turn
    or was pinned after the sweep selected it.
  - A client that disconnects during `/compact` or `/reflect` no longer leaves
    the session refusing turns with `409`.
  - A repeated ACP `session/load` no longer runs a restored queued prompt twice,
    and a `session/cancel` sent right after `session/prompt` cancels it.
  - Closing or failing to open a session releases its lease, so the next open
    no longer waits 35 s for a takeover.
  - A temporary file left by a crashed fork no longer makes retries of that
    fork fail.
  - Edits of `Foo.rs` and `foo.rs` on a case-insensitive file system are
    serialized.
  - chat-connector starts a new agent process when the ACP connection closes
    while the old process is still running, and no longer blocks other
    requests during the restart delay.
- Sub-agents can fork from an earlier prompt, yield while dependencies run,
  report verification outcomes and return persistent research reports. Delivery
  recovery distinguishes a still-running target from a completed result.
- Usage reporting distinguishes logical provider calls, transport attempts and
  token reports instead of inferring one from another.
- A long-running `otto serve` no longer retains memory or file descriptors per
  workspace load, delivery task, runner build, sub-agent task or written file.
  A workspace (one directory descriptor) is freed when its last runner closes;
  write locks, delivery approval state, stopped workflow bindings, session
  reviewers, dead writer handles and sub-agent report directory descriptors
  are released when no longer used. chat-connector deletes a session's prompt
  lock when no prompt holds or waits for it and splits an agent stderr line at
  64 KiB.

### Build and verification

- Code files have at most 800 lines and lines at most 200 characters, and
  files are not named `utils`, `types`, `config` or another generic name.
  Oversized files were split into domain-named modules, longer lines were
  wrapped, and local bindings in production code named `data`, `tmp`,
  `temp`, `obj`, `v4` or `bar` were renamed after their content, without
  behavior changes. `make code-limits`, `make check`, `make check-linux`,
  `make check-fast` and the pre-commit hook enforce the limits. To update an
  installed hook, delete `$(git rev-parse --git-path hooks)/pre-commit` and
  run `make setup-git-hooks`. See [code limits](docs/development/workflow.md#code-limits).
- A Rust source file is compiled only under `cfg(test)` exactly when its path
  has a directory or file named `tests` or `<domain>_tests`; the code limits
  check both directions. Nine test-only files were renamed:
  `cli::fixture_tests`, `subagent::doubles_tests`, `sandbox::conformance_tests`
  (with its five child files) and `seatbelt::driver::contract_tests`.
- `cargo test --workspace` no longer aborts with a stack overflow. In a debug
  build, the `embed` tests that run a turn needed up to 2.2 MB of thread stack,
  more than the 2 MiB default of a test thread or Tokio worker thread; they now
  need at most 1.03 MB.
- Rust is pinned to 1.99.1. Provider TLS and connector WebSocket dependencies
  receive security updates. `otto --version` includes the checkout commit.
- Rust dependencies are upgraded to their latest releases, including tokio
  1.54, sha2 0.11, jsonschema 0.59 and reqwest 0.13; base64 stays at 0.22.1. With reqwest 0.13, HTTPS
  certificates are verified by rustls-platform-verifier (the macOS trust
  evaluation; the system root store on Linux) over the aws-lc-rs provider,
  instead of rustls-native-certs roots over ring. `license-check` allows
  CDLA-Permissive-2.0 for webpki-root-certs, a wasm32-only dependency of the
  verifier. Rebuild with `make install`; no configuration change is needed.
- The repository has no CI. Run focused checks locally; full `make` gates are
  opt-in. The macOS gate includes PTY and core WASM tests; browser UI development
  remains in the separate [otto-plus](https://github.com/baiyuqing/otto-plus) repository.

## 0.4.8 - 2026-10-08

- Research tool bodies are bounded to 4 KiB; `read` provides a character cursor,
  and `grep` defaults to 30 matches. Child reports and status snapshots are
  bounded. ([#442](https://github.com/baiyuqing/otto/pull/442))
- Bash returns bounded previews with full redacted overflow logs for inspection.
  Logs and private child research directories are temporary: transcript paths
  may cease to exist after the turn and its children finish.
  ([#440](https://github.com/baiyuqing/otto/pull/440))
- `Ctrl+U` recalls queued guidance only after confirmed withdrawal, preserving
  newer drafts. Feature panels keep the conversation visible; approvals share
  one queue. ([#443](https://github.com/baiyuqing/otto/pull/443))
- ChatGPT requests use a session prompt-cache key. Automatic compaction also
  handles long-idle, sufficiently large contexts when enabled.
  ([#438](https://github.com/baiyuqing/otto/pull/438), [#441](https://github.com/baiyuqing/otto/pull/441))

## 0.4.5 - 2026-10-07

- AUTO reviews eligible requests against the original user task; explicit
  checkpoints and security permission changes still require a person.
  `/permissions` separates policy and reviewer settings; `never` rejects
  additional authority without prompting.
- Approval storage upgrades to version 4 after a backup. Use MANUAL to disable
  smart review; restore the backup before running an older binary.
- The TUI keeps queued next-turn input and current-turn guidance above the composer.

## 0.4.4 - 2026-10-07

- Sub-agent waiting becomes runtime-owned so dependency waits do not require
  repeated model polling. See [sub-agents](docs/manual/safety-and-tools.md#sub-agents).

## Earlier compatibility

- Session history remains append-only Pi v3 JSONL. Custom metadata must not be
  rewritten into older transcripts or exposed as ordinary model messages.
- Storage transfer is offline, resumable and explicit; it preserves original
  files and does not automatically clean them up. It is not a cloud backend.
  Consult the [storage manual](docs/manual/memory-and-extensions.md#storage-operations) before migration or downgrade.
- Only `openai-compatible` and `chatgpt` are supported providers. ChatGPT
  credentials come from `otto login`; API keys and connector secrets remain
  environment-only. macOS Seatbelt is the supported sandbox; Linux runs without it.
