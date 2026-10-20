# Changelog

Notable resulting changes, newest first. This is a curated summary, not a
commit ledger; Git and release tags retain the full history. See the
[user manual](docs/user-manual.md) for current behavior and the
[changelog rules](docs/development/workflow.md#changelog) for maintenance.

## Unreleased

### Usage and compatibility

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
- Optional remote memory supports authorized user/workspace/organization scopes,
  tenant-bound references and revision-checked mutations. `memory_search` can
  inspect by ID or browse a bounded effective set with `query: ""`; this is
  not a complete inventory. See [remote memory setup](docs/manual/memory-and-extensions.md#remote-memory-service).
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

- The TUI adds compact tool summaries (`Ctrl+O` for details), inline edit diffs,
  searchable skill management and `/trace` turn diagnostics. Queue banners keep
  pending guidance visible without replacing the conversation.
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
- Sub-agents can fork from an earlier prompt, yield while dependencies run,
  report verification outcomes and return persistent research reports. Delivery
  recovery distinguishes a still-running target from a completed result.
- Usage reporting distinguishes logical provider calls, transport attempts and
  token reports instead of inferring one from another.

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
- Rust is pinned to 1.99.0. Provider TLS and connector WebSocket dependencies
  receive security updates. `otto --version` includes the checkout commit.
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
