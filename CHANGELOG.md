# Changelog

Notable resulting changes, newest first. This is a curated summary, not a
commit ledger; Git and release tags retain the full history. See the
[user manual](docs/user-manual.md) for current behavior and the
[changelog rules](docs/development/workflow.md#changelog) for maintenance.

## Unreleased

### Breaking changes

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
  `otto memory status`. **Upgrade with all Otto processes stopped:** legacy
  SQLite memory must first be migrated with Otto 0.4.8; external memory routes
  must first be transferred back to local storage with 0.4.8. Remove
  `[storage.routes].memory`, then start this version for the one-time import
  of the schema-version-2 database. Check the imported files before manually
  deleting the old database. Memory is no longer a storage domain and is not
  moved by `otto storage transfer`. ([#491](https://github.com/baiyuqing/otto/pull/491))
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
- Rust is pinned to 1.99.0. Provider TLS and connector WebSocket dependencies
  receive security updates. `otto --version` includes the checkout commit.
- Rust dependencies are upgraded to their latest releases, including tokio
  1.54, turso 0.8.3, sha2 0.11, jsonschema 0.59 and reqwest 0.13; base64
  stays at 0.22.1, the version turso uses. With reqwest 0.13, HTTPS
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
