# Changelog

Notable changes to Otto, grouped by their effect on users and contributors.
Newest releases appear first; pending changes live under **Unreleased**.
Maintenance rules are in the [development guide](docs/development/workflow.md#changelog).

This initial backfill covers all 527 commits reachable from
[`89064ec`](https://github.com/baiyuqing/otto/commit/89064eca66b409007fac66c8c53f7fc138edc801),
from 2026-08-26 through 2026-10-05, including merged branch commits,
merge commits, reversions, tests, and documentation. Dates describe development
history, not version releases. The categorized summary describes the resulting
behavior; the [complete historical index](docs/changelog-history.md) records
individual changes, including experiments later removed. Current setup and
limitations are documented in the [user manual](docs/user-manual.md).

## Unreleased

### Usage and compatibility

- ChatGPT model discovery now sends a tested Codex catalog compatibility version
  instead of Otto's release version. Successful nonempty catalogs are cached in
  a private JSON file for three days, scoped to the endpoint, account and
  compatibility version; rejected model requests invalidate the cache.

- Sub-agent selection discovers the active provider's model catalog on demand
  for both ChatGPT and OpenAI-compatible connections, reusing it until the
  connection changes. The parent chooses an economical adequate model and gives
  a short reason; no manual list or built-in price ranking is required. Explicit
  choices and named-agent defaults retain precedence; the optional deprecated
  `subagent_models` override preserves its existing semantics. Failed discovery
  or unknown suitability uses the session model without blind retries. Task
  status preserves model-selection provenance, including compatibility with
  older tasks. See [sub-agents](docs/manual/safety-and-tools.md#sub-agents).

- A workspace initialized for the first time gets a readable memory scope ID,
  `<directory name>-<first 12 hex digits of the SHA-256 of its canonical path>`
  (for example `otto-60ef11ccec92`), instead of `sha256:<64 hex>`. The remote
  memory service receives this value as `workspace_id`. Existing
  `.otto/workspace.json` markers keep their `sha256:` ID. If such a marker is
  deleted, or the directory was last used by an Otto release without markers,
  the next start derives the new ID; map the path to the old ID under
  `[memory.workspace_ids]` to keep its records reachable. `otto workspace fork`
  assigns `<directory name>-<12 hex>` instead of a bare UUID. See
  [workspace identity](docs/manual/automation.md#workspace-identity).
  ([#507](https://github.com/baiyuqing/otto/pull/507))

- Remote `remember` accepts empty optional reference fields on a new memory,
  as some model calls supply them instead of omitting them.

- Remote memory organization ids now accept ordinary spaces, including names
  such as `bai's org`. The memory service must accept the same organization id.

- Remote memory replaces its pre-adoption v1 bodies in place: configure required
  `memory.remote.organization` and coordinate the external service/Otto client
  cutover before restart (no v2, default tenant or old-wire fallback). Personal
  memory is isolated per organization; keyed recall resolves workspace >
  organization > user, and filtered search selects effective winners rather than
  revealing shadowed values. Exact references are tenant-bound and remote updates/
  forget require opaque revisions. Pending writes are not persisted; organization
  remember is an explicit human, service-authorized operation, not a model tool.
  See [setup and cutover](docs/manual/memory-and-extensions.md#remote-memory-service)
  and the [canonical protocol](docs/manual/remote-memory-protocol.md).

- Remote `memory_search` now treats an empty optional `id` as omitted and reports the actionable `use either id or query, not both` error when both inputs are non-empty, matching the files backend.

- HTTP session creation and fork accept an optional `request_id`. Retrying after a lost response returns the original session, including after server restart. Reusing a fork request id with different arguments returns `409 request_id_conflict`; requests without an id keep their behavior.

- Memory records, tombstones and candidates are now plain files under
  `~/.otto/memory/` (`records/<id>.md`, `tombstones/<id>.json`,
  `candidates/<id>.json`, `identity.json`); the Turso memory database and its
  full-text index are removed. Hand edits to a record file take effect at the
  next read, invalid files are skipped and listed by `otto memory status`, and
  `otto memory` commands open the storage route authority only before the
  first import. Recall no
  longer matches the message text: after the standing preferences it injects
  the other in-scope records, workspace first and newest first, up to
  `max_results` and `recall_tokens`, so a workspace with records adds up to
  `recall_tokens` input tokens (default 2000) to each turn. `memory_search` and
  `/memory search` match any whitespace-separated term as a case-insensitive
  substring of text, kind, key or labels. `memory` is no longer a storage
  domain: it is not in `[storage.routes]`, `otto storage transfer` (no
  `--memory-db`), `storage init` or `storage migrate`, and `[storage.routes].memory`
  is a configuration error. `otto storage transfer` leaves `~/.otto/memory/`
  in the source home, keeps pending memory reviews, and refuses a route
  authority that an earlier release published with a `memory` route. `[memory].backend` defaults to `files`;
  `[memory.turso].path` is only the import source and `[memory.turso].busy_timeout`
  is the writer lock wait. Upgrade steps: stop all Otto processes; if memory is
  in a legacy SQLite `memory.db`, migrate it with `otto storage migrate` from
  Otto 0.4.8 first; if memory is on an external backend route, transfer it back
  to local storage with a 0.4.8 build first; remove `[storage.routes].memory`;
  start this version, which imports the schema version 2 database once and never
  modifies it; after checking the files, delete the old database manually. See
  [Memory](docs/manual/memory-and-extensions.md#memory). ([#491](https://github.com/baiyuqing/otto/pull/491))

- Slash commands are now recognized only when `/` is the first input character;
  leading whitespace keeps the text as an ordinary prompt. AUTO review no longer
  reports a human-blocked state to Herdr, avoiding false attention notifications.

- In the terminal UI, `/clear` now starts a fresh session immediately instead
  of opening a command panel. `/new` retains its existing panel.

- The terminal model picker now keeps profile and reasoning confirmations above
  the composer without escaped line breaks. ([#488](https://github.com/baiyuqing/otto/pull/488))

- Profiles can provide ordered `subagent_models` for cost-aware child tasks.
  An `agent` call without a model uses the first configured candidate after
  any named agent default; profiles without the list retain parent-model
  inheritance. See [configuration](docs/manual/getting-started.md#configuration).
  ([#485](https://github.com/baiyuqing/otto/pull/485))

- The TUI no longer recognizes or lists `/inputs` or `/inputs withdraw <id>`.
  Use ordinary text during an active turn or `/steer` for guidance, and `Ctrl+U`
  to recall queued guidance for editing. The composer banner and HTTP guidance
  APIs are unchanged. See [interactive use](docs/manual/interactive-use.md).
  ([#483](https://github.com/baiyuqing/otto/pull/483))

- Resuming a session no longer replaces the current profile, model or reasoning
  effort with historical session settings. In-process `/resume` keeps the
  current selection; startup `--resume` and `--continue` use the resolved
  configuration and command-line overrides. Historical model records remain
  available for tracing.

- When `TYPESAFE_API_KEY` is configured, automatic reflection now asks the
  auxiliary decision backend whether to run immediately or retain evidence for
  a later trigger. Manual and model-requested reflection bypass this scheduling
  hint; unavailable, timed-out or invalid decisions run the original reflection
  pipeline. Confidence never grants authority or bypasses review.

- In the terminal UI, `/rename <name>` now returns directly to the composer
  after a successful rename instead of opening an empty result panel. Bare
  `/rename` still opens the rename form.

- Agents can now submit `passed`, `failed`, `blocked`, `not_run` or
  `inconclusive` verification verdicts against prior tool results. Otto checks
  that cited evidence is durably recorded, terminal and structurally
  consistent with the verdict without requiring a particular test, lint or
  build command. This validates evidence integrity, not semantic coverage, and
  does not add a universal completion gate. See
  [verification verdicts](docs/manual/safety-and-tools.md#verification-verdicts).

- The terminal UI now shows only `/exit` in help and slash-command completion.
  The duplicate `/quit` spelling remains accepted as a compatibility alias.
  ([#475](https://github.com/baiyuqing/otto/pull/475))

- `/task` now covers all current-session sub-agent task operations: a bare
  `/task` lists tasks, `/task <id|name>` shows one, and `/task cancel
  <id|name>` cancels one. `/tasks` remains accepted as a compatibility alias
  for the list but is no longer shown as a separate command.

- Turns from a Telegram or Feishu chat and every turn of an `otto --prompt`
  run now stop after `[agent].unattended_turn_timeout`, default `30m`,
  counted per prompt step. Previously these turns had no time limit unless
  `turn_timeout` was set; alternating tool-call cycles could run until the
  process was killed. When both are set, the earlier of `turn_timeout` and
  `unattended_turn_timeout` applies.
  Set `unattended_turn_timeout = "0"` to restore the previous behavior. TUI,
  REPL, web UI and editor ACP turns are unchanged. See
  [configuration](docs/manual/getting-started.md#configuration).
  ([#469](https://github.com/baiyuqing/otto/pull/469))

- Approval `command_prefix` rules now check each command of a shell list or
  pipeline, parsed with tree-sitter-bash. An `allow` rule applies to
  `cargo fmt && cargo test` when every command starts with the prefix and the
  program has no expansions or other unsupported syntax. A `deny` or `ask`
  rule matches any command that starts with the prefix, including
  `git push origin "$BRANCH"`, which previously matched no prefix rule. See
  [approval rules](docs/manual/safety-and-tools.md).
  ([#461](https://github.com/baiyuqing/otto/pull/461))

- Under Seatbelt, a sandboxed Bash command that changes external state
  (`git push`, `gh` writes, `curl -X POST`, `npm publish`, `kubectl apply`
  and similar) now creates a `Boundary` approval request, and a command that
  deletes workspace or repository content (`git reset --hard`,
  `git clean -f`, `rm -r`, `find -delete`) creates a `Bounded` request. The
  command runs inside the sandbox after approval. AUTO reviews external-state
  commands; the `never` policy now rejects them unless an `allow` rule
  matches; deletions prompt only in MANUAL. See
  [classified commands](docs/manual/safety-and-tools.md#classified-commands-wait-for-approval-inside-the-sandbox).
  ([#462](https://github.com/baiyuqing/otto/pull/462))

- A command matching `[sandbox] excluded_commands` that changes external
  state or deletes workspace or repository content now waits for the same
  classified-command approval, then runs outside the sandbox. Excluded
  commands outside those classes, such as `lark-cli`, still run without an
  approval request. See
  [classified commands](docs/manual/safety-and-tools.md#classified-commands-wait-for-approval-inside-the-sandbox).
  ([#463](https://github.com/baiyuqing/otto/pull/463))

- `GET /v1/usage` now distinguishes token-usage rows, logical provider
  operations and actual HTTP attempts. Cache totals and `cache_hit_rate` are
  `null` when any reported usage lacks a cached-token breakdown; missing data
  is no longer presented as a zero cache hit. The former `requests` and
  `reported_requests` fields are named `usage_events` and
  `reported_usage_events` to match what they count.
  ([#447](https://github.com/baiyuqing/otto/pull/447))

### Features

- The TUI `/skill` command opens a searchable skill manager with highlighted
  keyboard navigation, scrollable read-only details, and enable/disable controls.
  Disabled skills remain manageable; saved configuration and current-session
  loading are shown separately, with saving errors and restart guidance. Existing
  text commands remain available. See [skills](docs/manual/memory-and-extensions.md#skills).
  ([#512](https://github.com/baiyuqing/otto/pull/512))

- The `agent` tool accepts `fork_before`, the text of an earlier user prompt.
  The sub-agent then starts with the parent conversation as it was before
  that prompt's turn, read from the session file, including turns compaction
  has since summarized. See
  [Sub-agents](docs/manual/safety-and-tools.md#sub-agents). ([#502](https://github.com/baiyuqing/otto/pull/502))

- `[memory] backend = "remote"` connects Otto to a memory service on another host
  over HTTP. Otto recalls per user input, sends explicit writes and `/remember`
  to the service, and uploads each finished turn; an incomplete or failed recall
  gives no memory context and the turn continues. The bearer token comes from
  `OTTO_MEM_TOKEN` (or `token_env`). See
  [Remote memory service](docs/manual/memory-and-extensions.md#remote-memory-service)
  and the [protocol](docs/manual/remote-memory-protocol.md). ([#493](https://github.com/baiyuqing/otto/pull/493))

- The terminal UI now shows colored inline diffs for successful `edit` and
  `write` calls. `write` records a bounded diff in the tool result, so resumed
  and attached sessions show the same change.
  ([#481](https://github.com/baiyuqing/otto/pull/481))

- `/trace` opens a panel with the timeline of a turn: provider requests,
  tool calls, compactions and sub-agents as a tree with durations and bars
  on a time axis, plus attempts, retries, time to first output, token usage
  and the text of each span. Each provider request of a turn or a
  compaction now appends an `otto.provider_call` session entry with its
  timing, retries, usage and request token estimates, and a sub-agent's
  `otto.task_spec` records the spawning call as `parentToolCallId`;
  sessions written earlier show estimated provider spans and no compaction
  spans. `GET /v1/sessions/{id}/trace` and
  `GET /v1/sessions/{id}/trace/{span_id}` serve the same data. While `/trace`
  is open, an approval request is announced in the panel and opened with `a`
  instead of being shown over it. See
  [`/trace`](docs/manual/interactive-use.md#slash-commands).
  ([#472](https://github.com/baiyuqing/otto/pull/472))

- Real terminal sessions now report Otto's root lifecycle with the OSC 7501
  Program Status Protocol. Supporting terminals can distinguish idle, working,
  approval-blocked, completed and failed turns without parsing screen content;
  redirected output remains free of escape sequences and reports contain only
  fixed, non-sensitive messages.
  ([#456](https://github.com/baiyuqing/otto/pull/456))

- Every provider request now passes through a local egress secret guard before
  transmission. Otto redacts known credentials and conservative high-confidence
  secret forms from safely rewritable text, and blocks requests whose structural
  or opaque fields cannot be changed safely; ordinary turns, compaction,
  sub-agents and auxiliary model calls share the same boundary.
  ([#455](https://github.com/baiyuqing/otto/pull/455))

- Native agent runtimes automatically retain private rotating diagnostic logs
  with session/task/operation IDs, HTTP and stream stages, active-operation
  heartbeats, outcomes and reported usage. Each runtime has an independently
  locked file family; content and credentials stay out of diagnostics and
  model context.
  ([#450](https://github.com/baiyuqing/otto/pull/450))

- Parent agents can explicitly yield when only child dependencies remain;
  settled tool results and existing mailbox wakeups preserve user input and
  completion races. Task detail distinguishes current activity from last tool.
  ([#448](https://github.com/baiyuqing/otto/pull/448))

- The ChatGPT Responses provider uses capability-gated remote compaction and
  persists the returned encrypted checkpoint as invisible append-only session
  state. Other providers, and manual compaction with a retention focus, keep
  using Otto's validated structured summaries.
  ([#447](https://github.com/baiyuqing/otto/pull/447))


### Fixes

- The terminal UI keeps queued next input pinned above the composer when an
  unrelated message arrives from the active turn. That message now appears in
  the transcript under its own text instead of consuming the queued input.

- A cross-workspace delivery whose target run was still `running` when
  `otto serve` stopped, for example after a crash, is now reported as
  `unknown` after restart, as documented, instead of `running` indefinitely.
  It is not resumed.
  ([#504](https://github.com/baiyuqing/otto/pull/504))

- After `/resume`, `/fork` or `/new` in the terminal UI, `↑`/`↓` recall the
  prompts of the session now open instead of those typed before the switch.
  The draft being typed is kept.
  ([#497](https://github.com/baiyuqing/otto/pull/497))

- Explicit `/sandbox exclude <entry>` in the terminal UI now reports success
  and errors inline instead of opening a result dialog that needs dismissal.
  Feedback identifies the persisted rule and immediate reload; duplicate rules
  remain deduplicated, and permission approvals are unchanged.
  ([#484](https://github.com/baiyuqing/otto/pull/484))

- Turns now stop unchanged failed cycles of 2–4 tool calls under the existing
  repetition limit, including rejected edits alternating with unchanged reads.
  Changed results break a cycle, and possible mutations reset cycle tracking.
  Compaction now asks for relevant, sourced observations and permits removing
  duplicate, superseded and resolved transient observations while retaining
  the full append-only session history.
  ([#480](https://github.com/baiyuqing/otto/pull/480))

- A provider stream that keeps sending bytes without model output now ends
  the attempt after 120 s on `chatgpt` and 300 s on `openai-compatible`.
  Previously the 60 s read timeout restarted on every received byte, so a
  stream that sent only keepalive data held the turn in responding until the
  user cancelled it. Only text, reasoning and tool-argument deltas restart the
  new timer. A `chatgpt` attempt is then retried under the existing policy with
  reason `model output idle timeout`; an `openai-compatible` attempt is retried
  only if no delta reached the caller. See
  [configuration](docs/manual/getting-started.md#configuration).
  ([#471](https://github.com/baiyuqing/otto/pull/471))

- Review facts for escalated bash commands cover every command in a list such
  as `make&&./run`, and read a leading `cd <dir>` and `git -C <dir>` from the
  parsed command instead of separate tokenizers.
  ([#461](https://github.com/baiyuqing/otto/pull/461))

- Pending memory proposals remain visible in `/memory review` after their
  originating session ends, so session-open reminders always lead to proposals
  that can be accepted or rejected. Review listing also pages past non-actionable
  storage candidates instead of letting them hide later actionable proposals.

- ACP memory review pagination now preserves the memory store's opaque snapshot
  cursor. If a proposal changes between pages, clients receive a conflict and can
  restart from an empty cursor instead of silently skipping another proposal.
  ([#460](https://github.com/baiyuqing/otto/pull/460))

- The TUI Memory Review collection now renders the selected candidate's full
  text, reason and advice in its detail pane instead of reducing that information
  to a one-line list preview. Loading failures and the explicit empty state remain
  visible with Retry or Esc recovery.
  ([#458](https://github.com/baiyuqing/otto/pull/458))

- Keep `otto_help` complete after the user manual split by embedding every
  topic page and indexing sections within their source page boundaries. A
  uniquely matching title phrase such as `Memory commands` now opens `Memory`;
  unknown or ambiguous topics include related section-title suggestions when
  available.
  ([#457](https://github.com/baiyuqing/otto/pull/457))

- Long sub-agent research can be handed off as persistent report files with a
  summary and section index. Oversized final answers are saved automatically;
  completion notifications and `agent_status` keep the full report's read path
  visible after truncation, so parents can retrieve omitted sections after the
  child exits. Failed `resume` calls now explain how to inspect completed tasks.
  ([#445](https://github.com/baiyuqing/otto/pull/445))

### Security

- The provider HTTPS client now uses `rustls` 0.23.46 instead of 0.23.44,
  which fixes RUSTSEC-2026-0285 (TLS 1.3 handshake messages accepted across
  encryption levels) and RUSTSEC-2026-0346 to 0349.
  ([#506](https://github.com/baiyuqing/otto/pull/506))
- `otto-connect` now uses `github.com/gorilla/websocket` v1.5.3 instead of
  v1.5.0 for the Feishu connection, which fixes GO-2026-6278 (WebSocket mask
  keys generated with a weak random number generator).
  ([#506](https://github.com/baiyuqing/otto/pull/506))

### Performance

- Implementation guidance settles shared interfaces and file responsibility
  before delegation, groups coherent edits, and emits one nonblocking batching
  advisory after repeated single replacements to one file.
  ([#448](https://github.com/baiyuqing/otto/pull/448))

- Context reporting and automatic compaction treat an opaque provider prefix
  as unknown until a post-checkpoint usage report anchors its size, rather than
  making cache and threshold decisions from a partial estimate.
  ([#447](https://github.com/baiyuqing/otto/pull/447))

### Build and CI

- The wasm32 test run in `make check` now also runs the two otto-core tests
  for replaying tool calls after a session takeover. They lacked the
  `wasm_bindgen_test` attribute, so wasm32 compiled them out and reported four
  dead-code warnings ([#505](https://github.com/baiyuqing/otto/pull/505)).

- Pin local and Docker builds to Rust 1.99.0, the current stable toolchain.
  Source-install instructions use the same version.
  ([#482](https://github.com/baiyuqing/otto/pull/482))

- `otto --version` reports the current commit when checkouts share a target
  directory. The build script ran `git` in the `CARGO_MANIFEST_DIR` captured
  when the script was compiled; Cargo reused that compiled script in other
  checkouts, so after the compiling worktree was deleted `git` failed, the
  build metadata became `unknown`, and no HEAD watch was registered to rebuild
  it. The script now runs `git` in the directory Cargo starts it in.
  ([#467](https://github.com/baiyuqing/otto/pull/467))

- The TUI PTY tests (`tui_pty`, `tui_attach_pty`) pass under `make check` run
  from an interactive terminal. The spawned `otto` kept that terminal as its
  controlling terminal, and crossterm read the window size from `/dev/tty`, so
  frames were drawn at the developer's terminal size instead of the 120x30
  test PTY and the startup wait for the composer border timed out. The test
  helper now starts the child in a new session, and a wait timeout no longer
  poisons the screen lock.

- Remove the GitHub Actions `Checks` workflow and its cache helper
  `scripts/ci-cache.mjs`. Pull requests no longer run the build, lint, test or
  Gitleaks history checks; `make check`, `make check-linux` and the
  pre-commit secret-scanning hook remain.
  ([#464](https://github.com/baiyuqing/otto/pull/464))

- Check persisted tool-result blocks directly in the batching-advice regression
  test, restoring the macOS and Linux CI gates without weakening its assertions.
  ([#453](https://github.com/baiyuqing/otto/pull/453))

- Classify ordinary Markdown as documentation regardless of its directory, so
  contributor `AGENTS.md` updates under `crates/` skip build and test gates;
  runtime `SKILL.md` and `AGENT.md` inputs still run the full gate.

- Normalize per-runtime diagnostic log identities in the embedded/ACP parity
  test while retaining file kinds and counts in the comparison.

- Fix the web retrieval byte limit expression to satisfy Clippy’s
  `manual_clamp` lint without changing its bounds.
  ([#446](https://github.com/baiyuqing/otto/pull/446))

### Documentation

- Add a documentation hub and split the user and development manuals into
  focused topic pages, with stable index pages that map legacy deep links.
  Archive the changelog's commit-by-commit history separately, refresh all
  repository-owned links, and correct the TUI, Docker UI, and local verification
  guidance.

- The manual documents remote compaction, provider-locked opaque checkpoints,
  exact operation/attempt counts, cache-breakdown presence and secret-safe
  runtime diagnostics.
  ([#447](https://github.com/baiyuqing/otto/pull/447))

- Contributor and agent instructions leave builds, tests and `make` gates to
  CI: local work runs only `cargo fmt --all`; a test's failure mode is shown
  by naming the assertion instead of reverting the change; opening the PR
  ends the task without polling CI; feature and fix PRs never edit version
  files; and the changelog PR link is added, committed and pushed in one
  command. ([#444](https://github.com/baiyuqing/otto/pull/444))

- `otto-core` holds I/O-free contracts, formats and the agent loop; `otto`
  holds their implementations. The code organization map lists the `otto`
  implementation of each core contract. `otto-core` tests fail on standard
  library filesystem, process, network, environment, stdio and wall-clock
  calls, and `otto` tests fail when `otto` source re-exports an `otto-core`
  item. ([#466](https://github.com/baiyuqing/otto/pull/466))

- Contributor and agent instructions run `cargo fmt --all` and the tests that
  cover the changed code before a Rust commit, and list each test command and
  its result in the PR body; the full `make` gates run only when asked.
  ([#466](https://github.com/baiyuqing/otto/pull/466))

## 0.4.8 - 2026-10-08

### Usage and compatibility

- Research retrieval now bounds `read`, `grep` and `web_fetch` bodies to 4 KiB;
  `read` supplies a character cursor and `grep` defaults to 30 matches.
  Parent-facing child reports are bounded, and `agent_status` omits recent steps
  unless requested. Children analyze downloaded sources in private temporary
  directories removed when their tasks exit; full transcripts remain local.
  ([#442](https://github.com/baiyuqing/otto/pull/442))

- Bash overflow logs are now temporary: guidance asks the model to remove them
  after analysis, and runtime cleanup removes owned logs once the turn and its child
  tasks finish. Old transcript paths may no longer exist after task completion.
  ([#440](https://github.com/baiyuqing/otto/pull/440))

### Fixes

- `Ctrl+U` now recalls queued current-turn guidance into an empty TUI composer
  after the runtime confirms withdrawal, so it can be edited and resubmitted.
  A new draft typed during the withdrawal is preserved, and guidance already
  delivered is never restored.
  ([#443](https://github.com/baiyuqing/otto/pull/443))

- TUI feature interactions now open as centered panels over the conversation
  instead of clearing and replacing the whole terminal. Approval decisions are
  laid out left-to-right, default to Approve/Accept, and use Left/Right; clicking
  a read-only Document no longer takes focus from the decision. Memory
  availability refreshes the unified approval card rather than opening a
  separate empty review screen, and a manually opened empty memory review states
  that no candidates are pending.

### Performance

- Agent guidance batches all known changes to one file into a single `edits`
  call and avoids parent rereads of files assigned to a sub-agent.
  ([#440](https://github.com/baiyuqing/otto/pull/440))

- Bash now returns bounded head/tail previews and saves complete redacted
  overflow logs in the workspace for targeted inspection. Execution status
  remains visible even when log storage fails. Agent guidance favors scripts
  that analyze logs locally and return checked summaries instead of full logs.
  ([#437](https://github.com/baiyuqing/otto/pull/437))
- The ChatGPT provider now sends `prompt_cache_key` with every request, set to
  the session id (a sub-agent uses its own transcript id), so the backend can
  route a session's requests to the same prompt cache. The OpenAI-compatible
  provider does not send it.
  ([#438](https://github.com/baiyuqing/otto/pull/438))
- Model guidance now tells the agent that each tool round trip resends the
  whole conversation: batch independent tool calls, read only the needed line
  ranges, and send `update_plan` in the same batch as the next tool call.
  ([#439](https://github.com/baiyuqing/otto/pull/439))
- With `compaction.auto` on, Otto now compacts once before a request when the
  last model response is at least 60 minutes old and the request estimate is
  above half of `working_window - reserve_tokens`. Local ChatGPT sessions
  showed no prompt-cache hits after more than 90 idle minutes.
  ([#441](https://github.com/baiyuqing/otto/pull/441))

## 0.4.5 - 2026-10-07

### Usage and compatibility

- The TUI now pins queued next-turn input and current-turn guidance immediately
  above the composer in a yellow `◆ QUEUED …` banner with a shaded background,
  so queued text remains visible while the transcript scrolls.

- AUTO now reviews requests for additional authority against the original user
  task, including shell escalations, MCP calls and ordinary persistent writes.
  Explicit checkpoints and security permission changes still require a user.
  `/permissions` exposes separate policy/reviewer controls; `never` rejects
  additional authority without prompting. Approval storage migrates to version
  4 after a backup; use MANUAL to disable smart review and restore the backup
  before running an older binary. See [unified approvals](docs/manual/safety-and-tools.md#unified-approvals).

## 0.4.4 - 2026-10-07

### Usage and compatibility

- Sub-agent waiting is now runtime-owned and non-blocking. The `agent` tool
  always returns immediately, child reports and completion wake an idle parent
  through its mailbox, and active guidance remains independently admissible.
  The blocking `agent_wait` capability and `agent`'s `wait` argument have been
  removed; use `agent_status` only for a progress snapshot, not polling. See
  the [sub-agent guide](docs/manual/safety-and-tools.md#sub-agents) and the
  [design](docs/specs/2026-10-07-nonblocking-parent-suspension.md).

### Fixes

- ChatGPT automatically retries transient failures after partial streamed output,
  preserving completed tools and marking the interrupted response before its
  replacement. Parent and sub-agent requests share bounded, cancellable recovery;
  session diagnostics record retry reasons and attempt counts
  ([#434](https://github.com/baiyuqing/otto/pull/434)).

- `agent` with `resume` accepts `context: "fresh"` (the default) as omitted,
  so a call that fills every field no longer fails with `resume does not
  accept context`.

- ChatGPT Responses streams no longer fail with `responses stream line
  exceeds 65536 bytes` when one event is larger than 64 KiB. Each event
  arrives on one SSE line, and `response.completed` repeats the request
  instructions and tool definitions plus all output, so a response with about
  20 KB of output exceeded the line cap. An SSE line may now hold one whole
  event up to the existing 1 MiB event cap ([#426](https://github.com/baiyuqing/otto/pull/426)).

- `request_approval` treats an empty `proposal` or `subject_ref` as omitted,
  so a call that sends a proposal with `subject_ref: ""` creates the approval
  instead of failing with "supply exactly one of proposal or subject_ref" ([#426](https://github.com/baiyuqing/otto/pull/426)).

- The `otto.turn` end entry of a failed turn now records the error that ended
  it (secrets redacted, at most 4 KiB), so a session file shows why a turn
  failed instead of only `"outcome":"failed"` ([#426](https://github.com/baiyuqing/otto/pull/426)).

- Bash now admits canonical Git-linked worktrees registered to the selected
  repository's exact common Git directory, without persistent `read_paths`
  approvals. Their Git and Otto identity metadata remain read-only; absent
  legacy read paths are skipped instead of disabling Bash.

- ChatGPT Responses streams now recognize transient overload/server errors
  across top-level, nested and SSE-event envelope variants, and retry a clean
  pre-output close with the existing bounded backoff. The REPL displays each
  planned retry, exhausted retries end the turn with an actionable error, and
  malformed streams cannot grow an individual SSE line or event without
  bound.

### Usage and compatibility

- Automatic skill contract checks are now a regular `[skills]` feature and
  default to enabled when `TYPESAFE_API_KEY` is set. Disable them with
  `[skills].typesafe_skill_check = false`; the old `[experimental]` setting is
  still accepted as a fallback. See the
  [user manual](docs/manual/memory-and-extensions.md#automatic-contract-check)
  ([#417](https://github.com/baiyuqing/otto/pull/417)).

- **A session can now decide every request: the YOLO approval mode.**
  `/auto manual|auto|yolo` (and `/auto on|off`, unchanged) selects the
  session's policy, `otto --yolo` starts one session in YOLO, and the HTTP and
  ACP mode APIs accept `{"mode":"yolo"}` alongside the boolean form.
  A YOLO session approves every request that carries its id — a plan review,
  an unsandboxed command, an external MCP call, a persistent sandbox read
  grant, a scheduled `prompt` item, a memory, skill or profile change, and its
  own child tasks — without a dialog; the recorded decision names `yolo-mode`.
  MANUAL and AUTO are unchanged, only a user action can set the mode, and a
  workspace-wide or other-session request still reaches the user. The mode
  lives in `approvals.db` (`user_version` 3), which migrates a stored boolean
  to the mode form with its meaning. See the
  [YOLO mode contract](docs/specs/2026-10-07-yolo-approval-mode.md) and the
  [user manual](docs/manual/safety-and-tools.md#unified-approvals).

- Otto's default system prompt now asks the model to structure and restate new
  action requests, resolve material ambiguity and wait for user confirmation
  before changing files or causing external side effects. Simple questions stay
  direct, confirmed scope does not need repeated confirmation, and an existing
  mandatory review can serve as that confirmation. This is prompt guidance;
  tool permissions still apply. See the
  [user manual](docs/manual/safety-and-tools.md#unified-approvals)
  ([#414](https://github.com/baiyuqing/otto/pull/414)).

- **A turn can now end by itself when it makes no progress.** A turn whose
  model repeats the same tool call, with the same arguments, 8 times in a row,
  or whose tool results fail 12 times in a row, ends with the error
  `no progress: ...` instead of running until a deadline — and the deadlines
  are unlimited unless configured. This applies to every turn, including each
  sub-agent's, which inherits the parent's limits. A turn that ends this way is
  not a persistence failure, so a frontend continues the session. The two
  limits are `[agent].no_progress_repeats` and `[agent].no_progress_failures`;
  `0` disables one condition and `0`/`0` disables the guard, which is the
  setting for a deliberate poll loop that repeats one identical call. See the
  [user manual](docs/manual/safety-and-tools.md#stopping-a-turn-that-makes-no-progress)
  ([#407](https://github.com/baiyuqing/otto/pull/407)).

- Session **AUTO** approval now decides only a *bounded authorization*: an
  operation that stays inside the sandbox and the workspace and grants nothing
  durable. A plan review (`request_approval`), an unsandboxed command, a
  cross-workspace delivery task, an external MCP tool call, a persistent
  sandbox read grant and a scheduled `prompt` item now always wait for the
  user in either mode instead of being approved automatically in AUTO. A
  request AUTO cannot decide is presented exactly as in MANUAL, and its
  recorded `pending` event is unchanged. See the
  [authority boundary contract](docs/specs/2026-10-07-auto-mode-authority-boundaries.md)
  and the [user manual](docs/manual/safety-and-tools.md#unified-approvals)
  ([#401](https://github.com/baiyuqing/otto/pull/401)).

### Features

- `edit` match errors now show where to look. When `old_text` matches more
  than once, the error lists the line numbers of up to 5 matches with the
  line before and after each match. When `old_text` is not found, the error
  shows the file lines that best align with `old_text`, ignoring leading and
  trailing whitespace, and how many `old_text` lines match there. The listing
  holds only current file lines, never `old_text`, and is capped at 4096
  bytes. In recorded sessions, 229 of 2825 `edit` calls failed with one of
  these two errors ([#423](https://github.com/baiyuqing/otto/pull/423)).

- Rust hosts can replace Otto's memory service through `MemoryExtension`,
  without implementing Otto's storage schema. The service-neutral core contract
  and host client enforce declared capabilities, scopes, reviewed preparations,
  explicit learning grants and honest operation receipts. The existing native
  recall/review path remains the default; external record-management views and
  a Mem0 transport are not included. See the
  [contract](docs/specs/2026-10-07-open-memory-contract.md)
  ([#419](https://github.com/baiyuqing/otto/pull/419)).

- Stop a sub-agent task from the model side with the new `agent_cancel` tool:
  it takes a task id or name and cancels that task, the same registry call
  `/task cancel`, `POST /v1/sessions/{id}/tasks/{task_id}/cancel` and ACP
  already reached. Cancellation stays cooperative — a queued task is released
  before it starts, a running one stops at its next checkpoint, and an
  effectful call settles instead of being abandoned — so the parent can stop a
  child that is stuck or no longer needed without waiting for it
  ([#407](https://github.com/baiyuqing/otto/pull/407)).

- Each turn now carries a progress guard that ends a turn which repeats one
  identical tool call, or keeps failing, past the configured limits. Two calls
  count as identical when the tool name and the arguments match; key order and
  spacing in the arguments are ignored. Only consecutive conditions are
  detected, so a cycle that alternates between different calls is not stopped
  and remains the case `agent_status` plus `agent_cancel` exists for
  ([#407](https://github.com/baiyuqing/otto/pull/407)).

- Steer the active turn from the TUI and take the guidance back. `/steer
  <guidance>` delivers guidance at the next safe boundary as an explicit
  command that never starts a turn, and `/inputs` lists what the active run has
  accepted with its message id and status (`queued`, `delivered`, `withdrawn`,
  `discarded`), so a still-queued item can be withdrawn with `/inputs withdraw
  <id>` or `Ctrl+U` for the newest one. Both work locally and with `--attach`,
  and mirror the existing HTTP `GET`/`DELETE /v1/sessions/{id}/tasks/steer`
  routes ([user manual](docs/manual/interactive-use.md#tui-behavior),
  [#404](https://github.com/baiyuqing/otto/pull/404)).
- `edit` now notes in its result when `new_text` drops the line break that
  ends `old_text`, so the model sees that the next line was joined onto the
  replaced text and can restore the break instead of leaving broken code.

### Fixes

- Approval review in the TUI now opens as a centered modal over the active
  conversation instead of replacing the whole screen. It has no feedback
  field, selects **Approve** by default, closes immediately after either
  successful decision, and presents concurrent requests one at a time in
  creation order instead of stacking them. Telegram and Feishu approval cards
  plus passive memory/reflection notices now follow the exact originating chat;
  workspace-visible or originless reviews remain explicitly discoverable but
  are no longer broadcast to every chat bound to the session or workspace. See
  [unified approvals](docs/manual/safety-and-tools.md#unified-approvals)
  ([#421](https://github.com/baiyuqing/otto/pull/421)).

- Compaction works again for sessions with `thinking` set on a model that
  returns reasoning, such as ChatGPT. The summary request inherits `thinking`,
  and the summary path cancelled the stream at the first reasoning delta and
  rejected the reasoning block, so every automatic and manual compaction
  failed. Each turn above the proactive trigger then sent one discarded summary
  request and the original request, and the context grew until the turn
  stopped at the hard input limit. Reasoning in a summary response is now
  ignored and is not stored in the checkpoint. The `compaction_warning` message
  and compaction errors now include the failure cause. See the
  [user manual](docs/manual/interactive-use.md#context-compaction)
  ([#420](https://github.com/baiyuqing/otto/pull/420)).

- Keep ChatGPT turns running through pre-output HTTP 429 and 500/502/503/504
  responses. Otto now retries the borrowed provider request up to six times
  with 1/2/4/8/16/30-second backoff, honoring a numeric `Retry-After` up to 30
  seconds; it never re-executes a tool or retries once output is visible.

- A process killed while it wrote the first records of a new session no longer
  leaves an empty or torn `<session-id>.jsonl`. Such a file failed to open
  ("session file is empty" or "session header must be one JSON object"),
  session lists skipped it, and `--continue` resumed an older session. The first records are now written to a `.tmp` file and given the
  session file's name after they are synced. A file torn by an earlier version
  still fails to open. See the
  [user manual](docs/manual/interactive-use.md#sessions)
  ([#416](https://github.com/baiyuqing/otto/pull/416)).

- Quitting Otto no longer fails with `otto: close sandbox: sandbox runtime
  close failed` and exit code 1 after a session whose sandboxed commands left
  read-only directories behind. A child creates them routinely: the Go module
  cache and Otto's own plugin store both install `0555` directories. The
  private state tree is removed through directory descriptors, and an entry can
  only be unlinked through a parent the owner may write and search, so removal
  stopped at the first such directory, deleted nothing else, and left the tree
  in `~/Library/Caches/otto-sandbox-*` — a leaked tree per affected session.
  Cleanup now restores the owner's access through the same descriptors it
  already validated against the recorded inode identities, then removes the
  tree; the identity checks and the symlink refusal are unchanged
  ([#408](https://github.com/baiyuqing/otto/pull/408)).
- Stop dropping the input typed during a turn without saying so. Ordinary text
  submitted while a turn runs is guidance for that turn, but the TUI never
  recorded it: `Ctrl+U` did nothing, the transcript never showed `Queued for
  current turn: …`, and the composer and transcript branches that advertised
  those clues were unreachable (`queued_input_sent` was set only by tests).
  Cancelling with `Esc`, a provider failure, or the turn deadline then
  destroyed the guidance silently, because a run end discards the guidance it
  never delivered. The TUI now shows the queued guidance while it waits,
  withdraws it with `Ctrl+U`, and every frontend reports a turn that ended
  without delivering it: once, as a visible `agent_steering` context message
  and a notification that quotes the message, with the text put back in the
  composer when it was empty. The unreachable queued-input withdrawal wiring
  this replaces (`queue_user_message`/`withdraw_user_message` from the TUI) is
  gone ([#404](https://github.com/baiyuqing/otto/pull/404)).
- The session AUTO policy no longer approves a request raised by the model on
  its own behalf: a plan or approach review is a human judgement, and letting
  the asking session answer it made the requesting model the approving party
  ([#401](https://github.com/baiyuqing/otto/pull/401)).

- A memory write whose commit fails with an error other than busy now
  reports "memory commit outcome is unknown" and closes the memory database
  connection, as the
  [state storage contract](docs/specs/2026-10-06-state-storage-contracts.md)
  requires. Before, the write issued `COMMIT` as a raw statement, so the
  failure was reported as an ordinary error and later writes kept using the
  connection ([#403](https://github.com/baiyuqing/otto/pull/403)).

### Security

- A request outside the sandbox boundary, a durable grant and a plan review
  cannot be approved by the session policy. The rule is enforced in the
  approval service and rechecked inside the committing transaction, and a
  request persisted without a declared authority class is treated as a user
  decision ([#401](https://github.com/baiyuqing/otto/pull/401)).

### Build and CI

- The `scripts/otto-tcp.sh` restart tests no longer fail with `ENOENT` on a
  pid file. The supervisor removes `serve.pid` or `connect.pid` when the child
  exits and writes it again after the restart; the test helper checked that
  the file existed and then read it, so a removal between the two calls threw
  instead of returning no pid to the polling loop. The helper now reads the
  file directly and treats `ENOENT` as no pid. Main's Linux checks failed this
  way after #427 ([#430](https://github.com/baiyuqing/otto/pull/430)).

- The chat connector end-to-end tests for memory cards, plan approval and
  approval cancellation run their chat as `telegram` again. Since #421 a
  card is shown only in the chat named by the approval's presentation
  origin, which the connector sends only for `telegram` and `feishu`; on the
  harness default platform `fake` no card was sent and the tests timed out
  ([#424](https://github.com/baiyuqing/otto/pull/424)).

- `make check-fast` runs its existing change selector directly, without a
  cross-worktree `lockf`/`flock` wrapper. Independent worktrees can start checks
  concurrently; the selected checks are unchanged. See
  [local verification](docs/development/workflow.md#local-verification)
  ([#419](https://github.com/baiyuqing/otto/pull/419)).

- Add test-only fault injection. The `fault-injection` Cargo feature exposes
  `otto::faults` to integration tests and no shipped build enables it;
  `tests/faults_boundary.rs` fails if production code names it. `faults::http`
  is one loopback HTTP server with scripted connection faults (refused, reset,
  stall, truncated or malformed stream) and replaces the servers copied into
  six test modules. `tests/fault_provider.rs` checks the attempt count, request
  count and effect certainty of both providers under those faults
  ([#402](https://github.com/baiyuqing/otto/pull/402),
  [development guide](docs/development/testing.md#fault-injection)). `faults::sql`
  wraps the SQL driver that every Turso repository now opens through, and
  `tests/fault_storage.rs` runs commit, rollback, open and read faults against
  each repository ([#403](https://github.com/baiyuqing/otto/pull/403)).
  `faults::files` wraps the session store's file driver, and
  `tests/fault_session.rs` runs write, sync and lock faults against session
  journals ([#406](https://github.com/baiyuqing/otto/pull/406)). The six atomic
  file writers (write and edit tools, reminder and inbox sidecars, workspace
  identity, configuration, reflection skill files, session forks) now share
  `fsops::publish_file`, and `faults::publish` injects create, write, sync and
  rename faults into it; reflection skill files now also sync their directory
  after the rename
  ([#409](https://github.com/baiyuqing/otto/pull/409)). `faults::sandbox` and
  `faults::mcp` wrap the sandbox driver and the MCP transport; new tests check
  that a failed launch is reported as not started, that a child ignoring
  `SIGTERM` is killed with its group after the grace on every driver, and that
  a hung MCP request times out, stopping a stdio server and leaving an HTTP
  client usable ([#413](https://github.com/baiyuqing/otto/pull/413)).
  `faults::crash` re-runs a test in a child process that exits at an injected
  fault, and the parent checks what the child left: a torn journal line, a
  takeover between its two renames, a fork, a configuration write, a skill
  revert, a turn-queue save and a workflow step commit. `tests/sigkill.rs`
  kills the `otto` binary during a bash call and checks that the resumed
  session reports the interrupted turn and does not run the command again
  ([#416](https://github.com/baiyuqing/otto/pull/416)).

- Main's `connect-check` no longer fails on the connector's end-to-end
  reflection test. That test scripted ten identical `read {"path":"notes.txt"}`
  calls to reach the ten successful operations automatic reflection waits for,
  but the progress guard ends a turn that repeats one call, with the same
  canonical arguments, eight times in a row — so the turn ended with
  `no progress: the same read call repeated 8 times in a row` and the test
  never saw a reflection. Both failing jobs, Linux checks and macOS acceptance,
  went red from #407 until each scripted call was given a distinct argument;
  the call count and the reflection assertion are unchanged
  ([#411](https://github.com/baiyuqing/otto/pull/411)).

### Documentation

- Archive the open Memory contract research in English, preserving source
  versions, short excerpts, verified facts and unknown guarantees. See the
  [research records](docs/specs/2026-10-07-open-memory-research/README.md)
  ([#419](https://github.com/baiyuqing/otto/pull/419)).

## 0.4.1 - 2026-10-07

### Features

- Keep a long-lived `otto serve` bounded: a session with no request, turn start
  or turn end for 30 minutes is closed, and once more than 64 sessions are open
  in the process the least recently used ones are closed too. Both defaults are
  on and configurable with `[server].session_idle_timeout` and
  `[server].max_open_sessions` (`"0"`/`0` turns one rule off). Closing a session
  this way is invisible to a client — it keeps its file, its history and its
  saved queued turns, and the next request that names it re-opens it from disk.
  Busy sessions are never closed, whatever their age or the count: a running or
  queued turn, a compaction, a reflection, a waiting approval, an unfinished
  sub-agent task, an undelivered notification, or a session a live ACP
  connection pinned. `DELETE /v1/sessions/{id}` still closes a session for
  good, `otto_sessions_evicted_total` counts automatic closes, and each one
  logs `session_evicted` ([#394](https://github.com/baiyuqing/otto/pull/394),
  [user manual](docs/manual/servers-and-connectors.md#session-lifetime)).

- Run the direct ACP TCP deployment as one supervised pair: `scripts/otto-tcp.sh
  start` runs `otto serve --acp-listen` and an `otto-connect` configured with
  `[agent] address`, so connect starts no `otto acp` process. It reads the same
  `otto.env` as the launcher, checks the token, the address, the token variable
  and the workspace before starting, starts connect only after serve announced
  its ACP listener, and offers `stop`, `restart [serve|connect]`, `status` and
  `logs`. A process that exits is restarted with backoff, and five consecutive
  fast failures stop the stack instead of looping. State and logs are mode
  `0600` under `~/.local/state/otto-tcp`; no secret is written
  ([user manual](docs/manual/servers-and-connectors.md#the-acp-tcp-stack-script),
  [#393](https://github.com/baiyuqing/otto/pull/393)).

- Embed Otto in another Rust program: `otto::embed::Otto` builds the same
  runtime `otto acp` builds (tools, sandbox, approvals, session files,
  warnings) in the host process, serves several sessions and workspaces, and
  lets the host switch off bash, MCP, memory, skills, plugins, sub-agents,
  reflection, schedule, session persistence, usage or task history at
  construction. The CLI, `otto serve` and `otto acp` run the same startup
  code, and their behavior is unchanged. The dependency is a git dependency
  ([design](docs/specs/2026-10-07-otto-embedding-api.md),
  [#392](https://github.com/baiyuqing/otto/pull/392)).

- Switch the conversation's profile from a chat with one tap: bare `/model`
  replies with the profiles from the local `~/.config/otto/config.toml` — name,
  provider and model each — and one inline button per profile in Telegram or one
  card button per profile in Feishu; choosing one switches that session exactly
  as typing `/model <profile>` does. A platform without buttons gets the same
  list as text. The connector reads that file read-only, so the picker shows the
  user layer only and marks the configured default rather than the session's
  current profile; switching itself stays Otto's
  ([#391](https://github.com/baiyuqing/otto/pull/391),
  [user manual](docs/manual/servers-and-connectors.md#commands)).

- Switch the model of one session from ACP itself with `/model [profile]`: Otto
  rebuilds the session's runner on another configured profile, makes no model
  request, and leaves the configuration file and every other session unchanged.
  A chat client connected directly to `otto acp` can therefore move off a
  profile whose quota is exhausted, which the attach-only `/config` commands
  cannot do. Bare `/model` reports the session's profile, provider, model,
  effort and the configured profile names; `otto acp --attach` runs the switch
  on `otto serve` through the new `POST /v1/sessions/{id}/profile` route
  ([#390](https://github.com/baiyuqing/otto/pull/390),
  [user manual](docs/manual/servers-and-connectors.md#compact-and-context-are-slash-commands)).

- Deliver work into a repository other than the session's own from an
  `otto serve` session: the parent-only `start_delivery_task` tool takes one
  authorization that binds the target checkout, its Git directory, base, new
  branch, worktree path and a frozen handoff of selected context, then runs the
  task in `TARGET/.otto/delivery-worktrees/<id>` as its own session and reports
  the result back, readable through
  `GET /v1/sessions/{source_session}/deliveries/{approval_id}`. The source
  workspace stays read-only, an interrupted delivery is reported as `unknown`
  instead of being replayed, ordinary sub-agents never receive the tool, and
  the Git/GitHub permission boundaries are unchanged
  ([design](docs/specs/2026-10-06-cross-workspace-delivery-design.md),
  [user manual](docs/manual/servers-and-connectors.md#cross-workspace-deliveries)).

- Report Otto's lifecycle to [Herdr](https://herdr.dev) when Herdr starts it in
  a pane: the pane becomes the agent `otto` as soon as Otto starts, `working`
  while a turn runs, `blocked` while a tool call waits for your decision, and
  `idle` when it waits for input. `herdr agent wait otto --until blocked`,
  `herdr agent prompt` and the sidebar therefore work for Otto, and the
  reported session file plus `otto --resume` command are what Herdr restores
  the session with. Nothing is reported outside a Herdr pane, and only the
  frontend that is the pane's agent reports: `otto serve`, `otto acp`,
  `--attach` and `--prompt` runs do not
  ([design](docs/specs/2026-10-07-herdr-agent-state.md),
  [#384](https://github.com/baiyuqing/otto/pull/384)).

- Start Otto from a source checkout with `scripts/otto.sh` or `make run`: the
  script reads `otto.env`, a gitignored shell file copied from the committed
  `otto.env.example`, and turns its settings — provider, model, profile, base
  URL, workspace, frontend, sandbox, thinking effort, timeouts and the API key
  variable — into the matching command-line options before executing the
  binary. Options written after the script name come last and win over the file,
  a missing `otto.env` still starts with Otto's own configuration, and
  `OTTO_ENV_FILE` and `OTTO_BIN` select another settings file or binary
  ([user manual](docs/manual/getting-started.md#launcher-script),
  [#389](https://github.com/baiyuqing/otto/pull/389)).

### User experience

- Rename the independent Go chat connector source directory to `chat-connector/`, making its Telegram/Feishu-to-ACP role clear while preserving the `otto-connect` command and existing `~/.otto/connect/state.json` state location.

- Make active-task steering observable and reversible before delivery: explicit
  steer responses now show the exact message and a transient id/status; clients
  can list and withdraw a still-queued steer. Delivered guidance remains
  append-only session history, while withdrawn or discarded guidance is never
  delivered to the agent.
- Make ordinary input during an active task steer that task at its next safe
  boundary across the local TUI, attached TUI, HTTP and ACP. Queueing an
  independent follow-up is now explicit; the new HTTP input-admission endpoint
  reports `steered`, `idle`, or `approval_pending` rather than silently queuing.
- Unify memory/reflection review with approval-style keyboard selection:
  memory review defaults to Accept, arrows select Accept or Reject, and Enter
  confirms; permission grants remain default Deny.
- Prevent a generic conversational confirmation from being requested before the
  exact memory review for the same model-originated `remember` proposal.
- Name why `otto acp --attach` and `otto --attach` cannot reach `otto serve`:
  the stderr line now carries the failure's causes, so a host without a
  running server reports `... (http://otto/healthz): client error (Connect): No
  such file or directory (os error 2)` instead of only `reqwest`'s request
  error. The line keeps its shape and the exit status stays `1`
  ([#388](https://github.com/baiyuqing/otto/pull/388)).

### Architecture and reliability

- Add public `otto_core::conformance` checks for the `Session` and `Mailbox`
  ports (default-on `conformance` feature), run against every session
  implementation and wrapper and the durable inbox adapter, including across
  a reopen. They found and fixed two bugs: session wrappers hid the operation
  ledger, so crash recovery could replay a tool call recorded as not
  read-only or already retried; and the session file forgot input ids from
  before a compaction checkpoint, so such an input could be delivered twice
  ([design](docs/specs/2026-10-06-session-and-mailbox-conformance.md)).
- Retry OpenAI-compatible requests whose connection could not be
  established, up to three times 1, 2 and 4 seconds apart within the turn
  deadline. Such a request never reached the endpoint, so it cannot run
  twice ([#378](https://github.com/baiyuqing/otto/pull/378),
  [user manual](docs/manual/getting-started.md#configuration)).
- Retry an OpenAI-compatible reply whose transport failed before any output
  arrived, such as `Connection reset by peer (os error 54)`, instead of
  failing the turn: three retries 1, 2 and 4 seconds apart within the turn
  deadline, each shown as `connection interrupted` on the status line. The
  endpoint may have started the completion, so a stream that does not recover
  is reported as interrupted with unknown effect, and a reply that was already
  streaming is never repeated. This matches what the ChatGPT transport already
  did ([user manual](docs/manual/interactive-use.md#tui-behavior)).
- Keep `task_queue` follow-ups across a crash: they are saved in
  `<id>.followups.json`, run in order after the turn that automatically
  resumes their task (a step that already ran is skipped), and are otherwise
  discarded with a visible notice. A follow-up that cannot be saved is
  rejected ([#377](https://github.com/baiyuqing/otto/pull/377),
  [user manual](docs/manual/interactive-use.md#sessions)).
- Make accepted inputs durable and deliver them once. Notifications carry
  stable input ids, so one appended just before Otto stops is not delivered
  again; guidance for a running turn is kept in `<id>.inputs.json` for that
  turn's automatic resume after a crash; unread `agent_send` messages survive
  until the sub-agent is resumed; and HTTP turns, inputs and steering accept
  an `input_id` (ACP `_meta.otto.inputId`, sent by otto-connect for every chat
  message) so client retries run once (`409 duplicate_input`). Inputs that
  cannot be saved are rejected (`503 input_not_persisted`, tool errors) and
  an unreadable inbox file is set aside with a warning instead of being
  emptied. Consumers now depend on the `Mailbox` port
  ([#376](https://github.com/baiyuqing/otto/pull/376),
  [user manual](docs/manual/interactive-use.md#sessions)).
- Recover sessions after a plain process crash the same way as after a lease
  takeover: read-only calls without results run again, a recovery notice lists
  what was interrupted, unfinished sub-agent tasks become resumable, and a
  wake turn continues the interrupted turn once. New `otto.turn` session
  entries tell an interrupted turn apart from a failed or cancelled one, each
  tool attempt records its retry safety so a reclassified tool is not replayed,
  `agent_status`, `remind_status` and `list_models` are now replayable, and
  `otto serve` reports resuming turns with trigger `recovery`
  ([#375](https://github.com/baiyuqing/otto/pull/375), [user manual](docs/manual/interactive-use.md#sessions)).
- Introduce a shared, WASM-safe `Mailbox` contract for agent notifications. Agent execution and runner-facing producers now depend on ordered snapshot and acknowledgement semantics instead of a concrete queue; the bundled in-memory mailbox remains the default, while native composition retains session-sidecar persistence and provides the extension boundary for alternative durable adapters.

### Storage and migration

- Expose registered external repository factories, environment-bound endpoints,
  namespace validation, portable migration codecs and contract checks. Keep
  local defaults and domain interfaces compatible; linked extension binaries
  can initialize fresh state or use verified offline transfer without reusing
  SQLite/Turso-specific SQL. No MySQL/PostgreSQL implementation is bundled.
- Separate native state semantics from SQL and POSIX persistence with typed
  domain ports, capability drivers, backend repositories, and runtime
  injection. Keep local Turso and confined POSIX storage as the defaults.
- Add resumable local state transfer with `otto storage transfer`, plus
  `otto storage abort` for pre-publication recovery. Keep
  `otto storage migrate <legacy.db> <new.db>` for recognized legacy imports.

### Usage and compatibility

- Rebuild TUI feature interaction around injected presenters, a command catalog,
  shared controls and an asynchronous local/attach driver. Navigation, focus,
  loading, errors and submission use one host; slow queries keep terminal input
  and live output responsive. `Esc` dismisses approvals without deciding, and
  bare `/approve` reopens them. Permission choices default to Deny; memory and
  reflection reviews default to Accept and support arrows plus Enter. Attached
  agent details consume the server's transcript rather than client filesystem
  paths.

- Add a native Gitleaks pre-commit hook and an independent CI history scan,
  including documentation changes, with redacted output and a pinned scanner.
  Exclude only 14 reviewed historical false positives by exact fingerprint.

- Move the browser UI and WASM bindings to [otto-plus](https://github.com/baiyuqing/otto-plus).
  Otto builds without a frontend bundle and serves only APIs; `/` and `/assets/`
  now return `404`, and `serve --open` is removed. Run the standalone frontend
  with its same-origin proxy and Otto's startup token. [#359](https://github.com/baiyuqing/otto/pull/359)

- Install versioned plugin packages from local directories or pinned Git commits.
  Load portable Skills and stdio/Streamable HTTP MCP, with limited Claude/Codex
  manifest adapters, namespaced components and explicit content-bound MCP trust.
  Manage packages with `otto plugin`; credentials stay in environment bindings.

- Keep the newest configuration backups when repeated writes land in the same
  millisecond; pruned collision names are no longer reused.

- Make the parallel bash redaction fixture register its notification waiters
  before signalling readiness, preventing a lost wakeup from hanging tests.

- Split repository agent instructions into a shorter root rulebook and scoped
  Rust-crate/documentation rules; keep detailed ownership and implementation
  contracts in the canonical contributor guides.

- Add managed > workspace-local > workspace-shared > user > defaults
  configuration, explicit scalar/table/list/named-map merge rules, source
  diagnostics and offline `config explain`. `config set` edits one explicit
  scope with the existing backed-up concurrent-change-safe writer. `--config`
  now replaces only the user source. Each server workspace resolves its own
  project files; project fields cannot redirect provider or MCP credentials.
- Require explicit `otto trust DIR --executables` for project MCP, skills and
  agents, including configured roots inside the project. Existing directory
  admission grants do not authorize executables. Content changes or revocation
  block future tool calls and request cancellation/MCP shutdown. Instructions
  now append global and ancestor files with 16 KiB/file and 64 KiB total limits.
- Enforce administrator-owned managed policy for provider/MCP destinations,
  credential bindings, bash confinement and selected resource ceilings across
  CLI/environment/session overrides and sandbox reload.

- Replace the macOS Tauri desktop shell with MyGo. Keep the existing Rust
  server, web UI, workspace picker, saved workspace and log locations. Desktop
  builds now use Go 1.27.1+; desktop contract and server lifecycle tests run in
  the local desktop gate and CI.

- Let reflection generate reusable Python scripts alongside `SKILL.md`. Script source
  is scanned, syntax-checked without execution, reviewed and included in the existing
  approval request. Complete skills are versioned and reverted together; human edits
  to scripts or the file set prevent automatic replacement. Markdown-only history
  remains compatible.
- Default session approvals to AUTO, with persistent `/auto on|off` controls,
  mode status, recorded request/decision notifications, and a session mode API.
  Telegram and Feishu support the controls; Telegram includes menu shortcuts
  and session-bound mode buttons.

- Replace the original Go runtime with Rust and a WebAssembly-safe shared core.
  Source builds use the pinned Rust toolchain and Make targets; the browser UI
  also needs Node and WASM tools. The earlier Go implementation is preserved
  at `go-final`. ([#92](https://github.com/baiyuqing/otto/pull/92))
- Rename the headless prompt flag from `--approve` to `--prompt`; update scripts
  that use the old flag. ([#153](https://github.com/baiyuqing/otto/pull/153))
- Replace native SQLite storage with Turso. Legacy SQLite config names remain
  aliases, but legacy FTS5 memory databases require an offline migration;
  search ordering may change. Stop Otto, run `otto storage migrate` into a new
  destination, then update the memory path. See
  [database migration](docs/manual/memory-and-extensions.md#migrating-an-existing-database).
  ([#295](https://github.com/baiyuqing/otto/pull/295))
- Remove `[inbound.feishu]`; configs containing it must delete the table and
  use `otto-connect` for Feishu chat access.
  ([#281](https://github.com/baiyuqing/otto/pull/281))
- Add Linux builds and execution. Linux has no confined shell driver, so `bash`
  requires explicit `--sandbox off`; macOS remains the supported platform with
  Seatbelt. ([#173](https://github.com/baiyuqing/otto/pull/173))
- Remove the agent's fixed maximum turn count; use cancellation and runtime
  deadlines to bound work. ([#7](https://github.com/baiyuqing/otto/pull/7),
  [#234](https://github.com/baiyuqing/otto/pull/234))
- Make MCP tools lazy by default and discover them through the router. Router
  calls require approval. ([#187](https://github.com/baiyuqing/otto/pull/187),
  [#324](https://github.com/baiyuqing/otto/pull/324))
- Public `web_fetch` and `web_search` access runs without per-host approval.
  Private-address and credential checks remain, and network deny disables
  web access. Legacy `[web].allow_hosts` entries are accepted but ignored.
  With `chatgpt` and no Google or Brave credentials, search uses ChatGPT's
  hosted tool, which does not use the local destination checks. See
  [web tools](docs/manual/safety-and-tools.md#web-tools).
  ([#328](https://github.com/baiyuqing/otto/pull/328))
- Remove the macOS desktop app (`desktop/`, `make desktop-app`,
  `make desktop-release`, `make desktop-check`) and `otto serve
  --exit-on-stdin-close`, which existed only for it. Use `otto serve --listen`
  with a browser instead. ([#355](https://github.com/baiyuqing/otto/pull/355))

### Features

- Keep queued prompts across a crash or restart: `otto serve` turns sent with
  `"queue": true` (including from the attached TUI and ACP) and local ACP
  `/queue` prompts are saved with the session before they are accepted.
  When the session is next opened, they run once each, in order, after the
  interrupted turn's automatic resume, keeping their serve turn ids, with a
  notice. A prompt that cannot be saved is refused (`503
  input_not_persisted`); cancelling or deleting removes it for good. Images
  in queued prompts are not saved
  ([design](docs/specs/2026-10-07-durable-turn-queue.md)).

- Let the model keep a short plan for multi-step work with the new
  `update_plan` tool: up to 20 one-line steps, each `pending`, `in_progress` or
  `completed`. The plan is saved with the session, so it survives Otto
  stopping, resume and fork, and compaction carries it in a `<plan>` block.
  The TUI shows `Plan 2/5 · <step>` above the status line and `/plan` shows
  the checklist; ACP clients receive standard `plan` updates, also on
  `session/load`; HTTP adds a `plan_updated` SSE event and the session
  object's `plan` field
  ([design](docs/specs/2026-10-07-session-plan.md)).

- Fork a session before an earlier prompt: TUI `/fork` (local and attached)
  lists the prompts that start a turn, writes a new session with the history
  before the chosen one, switches to it and puts the prompt in the editor.
  `POST /v1/sessions/{id}/fork` and `GET /v1/sessions/{id}/fork-points` do the
  same over HTTP, and the session object reports `forked_from`. The source
  session is never changed, a fork never ends inside a turn, and queued
  inputs, reminders and sub-agents stay with the source
  ([design](docs/specs/2026-10-06-session-fork.md)).

- Tell the model when a resumed session's tool set differs from the tool set
  recorded for it. At run start Otto appends a `capability_update` message,
  shown in the transcript, before the user message. It lists added and removed
  tools, or the current tools for sessions written before the record. Otto
  stores the tool names in an `otto.capabilities` session entry. Compaction
  carries summarized notices verbatim in a `<capability-updates>` block.
  ([#373](https://github.com/baiyuqing/otto/pull/373))

- Extend the resumed-session `capability_update` message to name tools whose
  description or parameters changed and the Otto version change. When the
  version changed and `otto_help` is available, it states that earlier
  `otto_help` results labelled with the old version describe the previous
  version. The `otto.capabilities` entry now also stores a digest per tool
  definition and the Otto version; entries from earlier versions produce no
  such lines. The duplicate `Shutdown` and `Configuration` manual headings
  under the agent server, ACP agent server and chat connector are renamed so
  `otto_help` can return each section.
  ([#374](https://github.com/baiyuqing/otto/pull/374))

- Open versioned lifecycle hooks and custom command tools on installed plugin
  snapshots, with sandbox execution, exact-argument authorization and
  append-only rewrite auditing. ([#358](https://github.com/baiyuqing/otto/pull/358))

- Let ACP clients attached to one `otto serve` select admitted workspaces per
  request. Route scheduled chat messages to each connector's declared chats;
  suspend ambiguous legacy delivery and recover when clients disconnect or
  their offers expire. Preserve scheduled items when upgrading the store.
  ([#357](https://github.com/baiyuqing/otto/pull/357))

- Ask the agent for a retrospective in plain words: the new `reflect` tool runs the same
  reflection as `/reflect` instead of the model writing its own file. Every reflection
  run now writes a short retrospective as the first section of its run log under
  `~/.otto/reflection/runs/`, and `/reflect` prints it after the result line.
  `/reflect list` and `/reflect show <id>` show past runs in every frontend, backed by
  `GET /v1/sessions/{id}/reflection/runs[/{run}]`. The system prompt also tells the model
  to create files with `write` rather than shell here-documents.

- Preserve workspace session and memory ownership across restarts and directory
  moves using a local identity marker. Add `otto workspace status|fork` for
  inspection and independent copies; adopt existing state without rewriting
  history. See [workspace identity](docs/manual/automation.md#workspace-identity).

- Establish the agent runtime, OpenAI-compatible streaming, TOML provider
  profiles, workspace file tools, cancellable shell execution, and a CLI REPL.
  ([initial implementation](https://github.com/baiyuqing/otto/compare/c582761...d1212b8))
- Add ChatGPT OAuth sign-in, model/profile selection, reasoning-effort controls,
  persisted session model settings, model listing, and first-run setup.
  ([#40](https://github.com/baiyuqing/otto/pull/40),
  [#132](https://github.com/baiyuqing/otto/pull/132),
  [#213](https://github.com/baiyuqing/otto/pull/213),
  [#242](https://github.com/baiyuqing/otto/pull/242))
- Add adaptive TUI/REPL modes, slash completion, session pickers, prompt history,
  folded tool details, context/token status, text selection, image input, and
  queued input that can guide an active turn. Scrollback experiments were
  reverted; the TUI uses the alternate screen.
  ([#2](https://github.com/baiyuqing/otto/pull/2),
  [#82](https://github.com/baiyuqing/otto/pull/82),
  [#123](https://github.com/baiyuqing/otto/pull/123),
  [#138](https://github.com/baiyuqing/otto/pull/138),
  [#235](https://github.com/baiyuqing/otto/pull/235))
- Add Pi v3 append-only session trees, workspace resume/listing, lazy session
  creation, archiving, renaming, manual/automatic context compaction, and Pi
  context-edit entries. ([#5](https://github.com/baiyuqing/otto/pull/5),
  [#8](https://github.com/baiyuqing/otto/pull/8),
  [#15](https://github.com/baiyuqing/otto/pull/15),
  [#34](https://github.com/baiyuqing/otto/pull/34),
  [#85](https://github.com/baiyuqing/otto/pull/85),
  [#241](https://github.com/baiyuqing/otto/pull/241))
- Add scoped memory storage and recall, revisioned records, human review of
  candidates, paginated management, local/chat review notifications, standing
  user preferences, and model advice before review.
  ([#30](https://github.com/baiyuqing/otto/pull/30),
  [#35](https://github.com/baiyuqing/otto/pull/35),
  [#245](https://github.com/baiyuqing/otto/pull/245),
  [#292](https://github.com/baiyuqing/otto/pull/292),
  [#306](https://github.com/baiyuqing/otto/pull/306),
  [#307](https://github.com/baiyuqing/otto/pull/307))
- Add on-demand skills, skill management and bundled skills, contracted skill
  delegation, and an experimental automatic contract check. Add reflection to
  propose memories and reviewed skill changes through terminal, HTTP, web,
  and chat, with per-run Markdown logs.
  ([#53](https://github.com/baiyuqing/otto/pull/53),
  [#180](https://github.com/baiyuqing/otto/pull/180),
  [#191](https://github.com/baiyuqing/otto/pull/191),
  [#255](https://github.com/baiyuqing/otto/pull/255),
  [#275](https://github.com/baiyuqing/otto/pull/275),
  [#303](https://github.com/baiyuqing/otto/pull/303),
  [#323](https://github.com/baiyuqing/otto/pull/323))
- Add named asynchronous sub-agents, model/context selection, task panels,
  cross-session task records, progress messages, wake turns, and natural
  active-task guidance. ([#57](https://github.com/baiyuqing/otto/pull/57),
  [#61](https://github.com/baiyuqing/otto/pull/61),
  [task records](https://github.com/baiyuqing/otto/commit/c731200),
  [#192](https://github.com/baiyuqing/otto/pull/192),
  [#308](https://github.com/baiyuqing/otto/pull/308))
- Add persistent reminder timers, scheduled deliveries and prompt runs,
  recurrence, retries, conversation write-back, and model-scheduled follow-ups.
  ([#114](https://github.com/baiyuqing/otto/pull/114),
  [#143](https://github.com/baiyuqing/otto/pull/143),
  [#314](https://github.com/baiyuqing/otto/pull/314))
- Add durable TOML workflows with approval gates, restart recovery, handoff,
  write coordination, and time travel.
  ([#159](https://github.com/baiyuqing/otto/pull/159),
  [#160](https://github.com/baiyuqing/otto/pull/160),
  [#161](https://github.com/baiyuqing/otto/pull/161),
  [#162](https://github.com/baiyuqing/otto/pull/162))
- Add Unix-socket and loopback HTTP serving, a browser UI with math/Mermaid,
  multi-workspace session management, live status, Git changes, and a macOS
  desktop shell. ([#54](https://github.com/baiyuqing/otto/pull/54),
  [#81](https://github.com/baiyuqing/otto/pull/81),
  [#89](https://github.com/baiyuqing/otto/pull/89),
  [#195](https://github.com/baiyuqing/otto/pull/195),
  [#198](https://github.com/baiyuqing/otto/pull/198),
  [#201](https://github.com/baiyuqing/otto/pull/201))
- Add MCP clients for stdio and Streamable HTTP servers, OAuth sign-in, and
  configuration management. ([#129](https://github.com/baiyuqing/otto/pull/129),
  [#140](https://github.com/baiyuqing/otto/pull/140))
- Add durable notifications, optional lease failover, takeover recovery and
  read-only tool replay; prevent simultaneous session writers.
  ([#208](https://github.com/baiyuqing/otto/pull/208),
  [#221](https://github.com/baiyuqing/otto/pull/221),
  [#222](https://github.com/baiyuqing/otto/pull/222),
  [#223](https://github.com/baiyuqing/otto/pull/223),
  [#261](https://github.com/baiyuqing/otto/pull/261))
- Add ACP serving, TUI/ACP attach modes sharing `otto serve` sessions, and
  `otto-connect` for Telegram/Feishu. Support chat profile management,
  approvals, memory review cards, Markdown replies, slash commands, and
  authenticated loopback ACP TCP connections.
  ([#262](https://github.com/baiyuqing/otto/pull/262),
  [#272](https://github.com/baiyuqing/otto/pull/272),
  [#279](https://github.com/baiyuqing/otto/pull/279),
  [#280](https://github.com/baiyuqing/otto/pull/280),
  [#289](https://github.com/baiyuqing/otto/pull/289),
  [#302](https://github.com/baiyuqing/otto/pull/302))
- Add agent self-help and pending approval withdrawal, provider/context
  metrics, persistent token usage, startup traces, process logs, request
  correlation, and connector metrics.
  ([#78](https://github.com/baiyuqing/otto/pull/78),
  [#127](https://github.com/baiyuqing/otto/pull/127),
  [#165](https://github.com/baiyuqing/otto/pull/165),
  [#298](https://github.com/baiyuqing/otto/pull/298),
  [#327](https://github.com/baiyuqing/otto/pull/327))
- Add bounded workspace search and robust edit replacements with compact
  diffs. Add `web_fetch` for text/Markdown extraction and pagination, and
  `web_search` using Google, Brave, ChatGPT hosted search, or Bing.
  ([#10](https://github.com/baiyuqing/otto/pull/10),
  [#12](https://github.com/baiyuqing/otto/pull/12),
  [#90](https://github.com/baiyuqing/otto/pull/90),
  [#328](https://github.com/baiyuqing/otto/pull/328))

### Fixes

- Keep the TUI footer's token totals and cached-prompt share visible on a
  narrow terminal, and show the cache hit rate beside the cached count
  (`tokens 7.2M/37.5k (cached 7.1M, 98.7%)`). The footer ordered the model,
  the reasoning effort, the approval mode, the sandbox state and the
  workspace before the token totals, so ratatui clipped the totals off the
  right edge whenever those fields were long; a `deepseek-v4-flash` session
  lost them where a shorter ChatGPT model id did not. The footer now lists
  the model, the approval mode, the token totals, the context percentage,
  the sandbox state, the reasoning effort, the workspace and the session ID
  in that order, so only the tail is dropped.
  [#396](https://github.com/baiyuqing/otto/pull/396)

- Clarify the mutually exclusive `edit` argument forms with JSON examples and
  tell models how to correct mixed batch/single arguments before any file write.
  [#362](https://github.com/baiyuqing/otto/pull/362)

- Keep resumed sessions in the requested workspace, resolve new-session and
  workflow runtimes from that workspace's configuration, and wire its own
  sandbox control and persistent read approvals. Rebind ACP notifications
  when a session is loaded again after being reopened. Preserve ACP invalid-parameter
  errors when workspace admission rejects a request. ([#357](https://github.com/baiyuqing/otto/pull/357))

- Make zsh here-documents (`cat > file <<EOF`) work in the Seatbelt sandbox.
  The sandbox now sets `TMPPREFIX` to its private temporary directory, so zsh
  no longer tries to write the denied `/tmp/zsh*` and leave an empty target file.

- Compare the layered-configuration directory chain against canonical paths in its
  test, so it passes where the temporary directory is reached through a symbolic link,
  as on macOS.

- Select manual approval mode in the macOS interactive elevation test so it
  continues to verify explicit approval with sessions defaulting to AUTO.

- Remove Telegram approval mode buttons after a successful selection; the
  original card shows the resulting AUTO or MANUAL mode, including when selecting
  the already active mode. Use `/auto` to open fresh controls.

- Telegram AUTO/MANUAL buttons update the original mode card; repeated clicks
  no longer send duplicate cards. ([#344](https://github.com/baiyuqing/otto/pull/344))

- ChatGPT stream errors with code `server_is_overloaded` or `server_error`
  are now retried like transport failures (up to three times, 1, 2, then 4
  seconds) when no output has been streamed yet. Previously the turn failed on
  the first such error.
- Under Seatbelt, the `bash` tool description now states that `HOME` and `~`
  are a private sandbox directory, so the model uses absolute paths for the
  user's files instead of reading `~/...` from the private home.
- `web_fetch` and `web_search` now work on Macs where a TUN interface answers
  DNS with fake IPs from 198.18.0.0/15. The tools blocked those answers as
  non-public, so every request failed.
  ([#337](https://github.com/baiyuqing/otto/pull/337))
- `BRAVE_API_KEY`, `GOOGLE_API_KEY` and `GOOGLE_SEARCH_ENGINE_ID` now take
  effect. They were not passed to the web tools, so `web_search` always used
  ChatGPT hosted search or Bing. ([#335](https://github.com/baiyuqing/otto/pull/335))
- Errors reported inside a ChatGPT response stream now include the server's
  error code and message instead of only `responses stream reported error`.
  ([#336](https://github.com/baiyuqing/otto/pull/336))
- Harden frontend/session replacement, cancellation and close ownership,
  resume identity, tool ordering, empty message/output handling, cached usage,
  compaction cuts and summaries, and checkpoint reconciliation. Individual
  regression fixes are linked in the historical index.
- Fix TUI IME/caret placement, scrolling, slash suggestions, overlays, status
  bars, typing/history/paste handling, queued-input rendering, and warnings
  corrupting terminal output. Fix web wake turns, image-only turns, image
  paste, image rendering, rename dialogs, and readable code display.
- Fix edit ambiguity, whitespace preservation, lock aliasing, and empty
  argument handling. Reject BOM-only `old_text` instead of panicking and
  limit fuzzy trailing-whitespace matching to line endings.
  ([#184](https://github.com/baiyuqing/otto/pull/184),
  [#329](https://github.com/baiyuqing/otto/pull/329))
- Fix concurrent/inherited session locks, active-session archiving, duplicate
  picker identities, and config writes; preserve config formatting and back
  up changes with compare-and-swap coordination.
  ([#152](https://github.com/baiyuqing/otto/pull/152),
  [#170](https://github.com/baiyuqing/otto/pull/170),
  [#210](https://github.com/baiyuqing/otto/pull/210),
  [#211](https://github.com/baiyuqing/otto/pull/211),
  [#215](https://github.com/baiyuqing/otto/pull/215),
  [#217](https://github.com/baiyuqing/otto/pull/217))
- Retry eligible ChatGPT transport failures with visible progress. Fix ACP
  prompt admission races and deliver wake-turn replies to ACP/chat clients.
  ([#293](https://github.com/baiyuqing/otto/pull/293),
  [#312](https://github.com/baiyuqing/otto/pull/312),
  [#313](https://github.com/baiyuqing/otto/pull/313),
  [#315](https://github.com/baiyuqing/otto/pull/315))
- Fix exact memory deletion lookup, blank schedule fields, one-time recurrence,
  escaped tool argument displays, and the macOS log follower surviving Otto.
  ([#310](https://github.com/baiyuqing/otto/pull/310),
  [#319](https://github.com/baiyuqing/otto/pull/319),
  [#320](https://github.com/baiyuqing/otto/pull/320),
  [#325](https://github.com/baiyuqing/otto/pull/325))

### Security

- Recheck chat admission when restoring connector bindings and delivering
  background replies, so removed chats no longer receive session notices. ([#357](https://github.com/baiyuqing/otto/pull/357))

- Confine file tools to the workspace, validate canonical paths and symlinks,
  protect session resume identities, and bound file reads, edits, prompt files,
  search traversal, and terminal strings.
  ([#5](https://github.com/baiyuqing/otto/pull/5),
  [#19](https://github.com/baiyuqing/otto/pull/19),
  [#68](https://github.com/baiyuqing/otto/pull/68))
- Add default macOS Seatbelt execution, interactive setup and reload, approved
  read paths/network access, and one-shot elevated-shell approvals. Make Git
  metadata read-only and surface sandbox denials in failed shell results.
  ([#44](https://github.com/baiyuqing/otto/pull/44),
  [#111](https://github.com/baiyuqing/otto/pull/111),
  [#151](https://github.com/baiyuqing/otto/pull/151),
  [#259](https://github.com/baiyuqing/otto/pull/259),
  [#260](https://github.com/baiyuqing/otto/pull/260))
- Redact secret-bearing provider output and JSON, harden provider HTTP clients
  and trace capture, patch dependencies, and clarify untrusted prompt content.
  ([#19](https://github.com/baiyuqing/otto/pull/19),
  [#52](https://github.com/baiyuqing/otto/pull/52),
  [#68](https://github.com/baiyuqing/otto/pull/68),
  [#244](https://github.com/baiyuqing/otto/pull/244))
- Close sandbox/inbound authorization gaps, keep Bash approvals pending until
  resolved, and unify human approval authority across runtimes and frontends.
  Harden chat configuration, image handling, prompt schedules, and persistent
  approval decisions. ([#150](https://github.com/baiyuqing/otto/pull/150),
  [#246](https://github.com/baiyuqing/otto/pull/246),
  [#316](https://github.com/baiyuqing/otto/pull/316),
  [#326](https://github.com/baiyuqing/otto/pull/326))
- Local web tools block non-public destinations, check input for secrets on
  a best-effort basis, ignore proxy environment variables, and refuse
  cross-host redirects and HTTPS-to-HTTP downgrades. Local web results are
  untrusted and excluded from reflection evidence.
  ([#328](https://github.com/baiyuqing/otto/pull/328))

### Performance

- Reduce per-request prompt cost, connect MCP servers concurrently, start the
  interactive frontend before MCP connections finish, shorten interactive
  exit, and compact skill listings.
  ([#52](https://github.com/baiyuqing/otto/pull/52),
  [#168](https://github.com/baiyuqing/otto/pull/168),
  [#169](https://github.com/baiyuqing/otto/pull/169),
  [#171](https://github.com/baiyuqing/otto/pull/171),
  [#250](https://github.com/baiyuqing/otto/pull/250))
- Stop idle ACP polling from consuming CPU.
  ([#318](https://github.com/baiyuqing/otto/pull/318))
- Compile Turso dependencies at optimization level 2 in development/test
  builds to speed up storage-heavy tests.
  ([#330](https://github.com/baiyuqing/otto/pull/330))

### Build and CI

- Fail the Linux gate when a shipped Rust crate or web UI npm package declares
  a license outside the allowlist (MIT, Apache-2.0, BSD-2-Clause,
  BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense); `make check-fast` runs the
  check when a manifest or lock file changes.
  ([#354](https://github.com/baiyuqing/otto/pull/354))

- Add a multi-stage Docker image with the complete embedded Web UI and a
  Compose setup for Docker Desktop on Apple Silicon and Intel Macs. Persist
  configuration and sessions, run as a non-root user, and preserve Otto's
  loopback listener and API token through a TCP relay.
  ([#353](https://github.com/baiyuqing/otto/pull/353))

- Fix the stale ACP command-list test expectation after adding `/auto`; verify
  the command is advertised for new, loaded, and attached sessions. Approval
  interaction tests explicitly select MANUAL now that AUTO is the default.

- Establish canonical Make gates, offline behavioral/contract tests, PTY
  integration coverage, Rust/WASM boundary checks, browser UI tests, Linux
  checks, and Go connector race tests. Skip full gates for docs-only changes.
  ([#71](https://github.com/baiyuqing/otto/pull/71),
  [#92](https://github.com/baiyuqing/otto/pull/92),
  [#173](https://github.com/baiyuqing/otto/pull/173),
  [#177](https://github.com/baiyuqing/otto/pull/177),
  [#272](https://github.com/baiyuqing/otto/pull/272))
- Build the current browser UI for embedding, detect `ui/dist` changes, install
  Otto and the connector to local bin, install bundled skills, and add local
  ad-hoc signed desktop builds.
  ([#122](https://github.com/baiyuqing/otto/pull/122),
  [#126](https://github.com/baiyuqing/otto/pull/126),
  [#149](https://github.com/baiyuqing/otto/pull/149),
  [#202](https://github.com/baiyuqing/otto/pull/202),
  [#282](https://github.com/baiyuqing/otto/pull/282))
- Embed the Git commit in the version and add coordinated version bumps,
  release tagging and GitHub release creation.
  ([#322](https://github.com/baiyuqing/otto/pull/322))
- Remove duplicate builds and PTY runs, reuse workspace test artifacts for
  connector checks, cache Cargo dependencies and versioned WASM tools
  separately, and reduce CI debug information/incremental overhead.
  ([#321](https://github.com/baiyuqing/otto/pull/321),
  [#330](https://github.com/baiyuqing/otto/pull/330))
- Stabilize process, connector, lease, PTY, log, and fixture tests; replace
  avoidable real-time waits with virtual time or explicit synchronization.
  ([#299](https://github.com/baiyuqing/otto/pull/299),
  [#309](https://github.com/baiyuqing/otto/pull/309),
  [#330](https://github.com/baiyuqing/otto/pull/330))
- Run macOS formatting and clippy in their own CI job beside the tests, with
  their own dependency cache; `make check-tests` is `make check` without
  them. ([#332](https://github.com/baiyuqing/otto/pull/332))
- Make `make check-fast` the local gate: it runs only the checks for the paths
  changed since `origin/main` (formatting, clippy, the changed `crates/otto`
  modules' unit tests, changed integration tests, and the UI, connector or
  script checks when those directories change), and a lock in the common git
  directory lets it run in one worktree at a time. Local work no longer runs
  `make check`; CI runs the full gates.
  ([#341](https://github.com/baiyuqing/otto/pull/341))

### Documentation

- Make the release contract patch-only: a release increments the patch number
  and a new feature no longer raises the minor version, so contributors bump
  `X.Y.Z-1` to `X.Y.Z` in a release PR instead of planning a minor.
  ([#400](https://github.com/baiyuqing/otto/pull/400),
  [development guide](docs/development/workflow.md#releases))

- Add a mandatory contributor rule: a PR that adds, changes, removes or
  renames a model-callable capability updates its tool description, schema
  field descriptions, prompt policy section, sub-agent exclusion and user
  manual. Descriptions are short and unambiguous and state the capability,
  inputs and outputs. ([#373](https://github.com/baiyuqing/otto/pull/373))

- Add and maintain English/Chinese README guidance, the user manual,
  architecture/development contracts, the code organization map, and historical
  design documents. Align instructions with implemented behavior, clarify
  Otto's general-purpose identity, and refresh logos and startup branding.
  ([#31](https://github.com/baiyuqing/otto/pull/31),
  [#66](https://github.com/baiyuqing/otto/pull/66),
  [#164](https://github.com/baiyuqing/otto/pull/164),
  [#288](https://github.com/baiyuqing/otto/pull/288),
  [#296](https://github.com/baiyuqing/otto/pull/296))
- Introduce this classified changelog, require updates for notable changes,
  and document release archival.
  ([#333](https://github.com/baiyuqing/otto/pull/333))

## Historical index

The fixed commit-by-commit backfill is archived in the [complete historical index](docs/changelog-history.md).
