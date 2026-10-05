# Changelog

Notable changes to Otto, grouped by their effect on users and contributors.
Newest releases appear first; pending changes live under **Unreleased**.
Maintenance rules are in the [development guide](docs/development.md#changelog).

This initial backfill covers all 527 commits reachable from
[`89064ec`](https://github.com/baiyuqing/otto/commit/89064eca66b409007fac66c8c53f7fc138edc801),
from 2026-08-26 through 2026-10-05, including merged branch commits,
merge commits, reversions, tests, and documentation. Dates describe development
history, not version releases. The categorized summary describes the resulting
behavior; the [complete historical index](#complete-historical-index) records
individual changes, including experiments later removed. Current setup and
limitations are documented in the [user manual](docs/user-manual.md).

## Unreleased

### Usage and compatibility

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
  [database migration](docs/user-manual.md#migrating-an-existing-database).
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
  [web tools](docs/user-manual.md#web-tools).
  ([#328](https://github.com/baiyuqing/otto/pull/328))

### Features

- Preserve workspace session and memory ownership across restarts and directory
  moves using a local identity marker. Add `otto workspace status|fork` for
  inspection and independent copies; adopt existing state without rewriting
  history. See [workspace identity](docs/user-manual.md#workspace-identity).

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

## Complete historical index

This fixed backfill accounts for every commit through `89064ec`, once each.
Entries are grouped by their primary category, newest first within a category;
multifaceted changes may appear in more than one summary above. Commit links
preserve the original diff and subject. Subjects below are shortened for
readability. Design-only entries, reverted experiments, and merge records are
historical evidence, not claims of current features. Future PRs update the
curated entries under `Unreleased`; this snapshot stays fixed.

<details>
<summary>Usage and compatibility (11 commits)</summary>

- 2026-10-03 — [a77bbd0](https://github.com/baiyuqing/otto/commit/a77bbd08a8907ea0123e52ea89446113755d62ee): Replace native SQLite storage with Turso (#295)
- 2026-10-02 — [782d9d5](https://github.com/baiyuqing/otto/commit/782d9d502abf3c04a2aa4e280770a098e8e7a38c): Remove [inbound.feishu] and the inbound module (#281)
- 2026-09-25 — [c04689d](https://github.com/baiyuqing/otto/commit/c04689d48f80e34eb2ab63d619ced4975d367ebf): Make MCP tools lazy by default (#187)
- 2026-09-22 — [6fb7da5](https://github.com/baiyuqing/otto/commit/6fb7da5b5913874e4e0c887dd18da134b4a362dd): Build and run Otto on Linux, with no sandbox (#173)
- 2026-09-21 — [f41c073](https://github.com/baiyuqing/otto/commit/f41c0733e0338c57e514bb287ce841a040c38b88): Rename --approve to --prompt and name the user in the elevated-Bash refusal (#153)
- 2026-09-20 — [4a7e6ea](https://github.com/baiyuqing/otto/commit/4a7e6ea7e76bc17de1ba3712aea071192d7a2923): Revert "Move the TUI transcript into the terminal's scrollback" (#138)
- 2026-09-14 — [25c6122](https://github.com/baiyuqing/otto/commit/25c61222bc37a829549f7896d259946888100d39): Rewrite Otto in Rust with a WebAssembly core (#92)
- 2026-09-06 — [5be5cf9](https://github.com/baiyuqing/otto/commit/5be5cf98161b7241099bd2cd130aa52d14bff396): Restore the alternate-screen transcript (#82)
- 2026-08-31 — [dbfd28b](https://github.com/baiyuqing/otto/commit/dbfd28bfe2524eb3a41432a51d982f968a0c2a90): Revert "feat: inline REPL with native text selection (#36)"
- 2026-08-28 — [b701f22](https://github.com/baiyuqing/otto/commit/b701f22ed8b0ced76d72d429f59d14afbf3ff621): Remove agent max-turns limit
- 2026-08-27 — [1e0989c](https://github.com/baiyuqing/otto/commit/1e0989cbd6e79104dfaab3a0473e3c55fa0ef132): Raise default agent max turns to 50

</details>

<details>
<summary>Features (224 commits)</summary>

- 2026-10-05 — [b3c4f42](https://github.com/baiyuqing/otto/commit/b3c4f426b8fc65cd873bc405691276812f63a07b): Add web_fetch and web_search tools (#328)
- 2026-10-04 — [fed97a4](https://github.com/baiyuqing/otto/commit/fed97a492629c15085c5a4eff43324c7c2085889): Add request correlation, step log events and connector metrics (#327)
- 2026-10-04 — [a0d162b](https://github.com/baiyuqing/otto/commit/a0d162b26aeee6dbeca786e2f3c90b441e70341a): Write a Markdown log for every reflection run and expand ~ in the memory path (#323)
- 2026-10-04 — [0759e93](https://github.com/baiyuqing/otto/commit/0759e93e8addf28723e0b0d3c24c2db88f2fa5ba): Treat blank schedule fields as absent and add repeat "once" (#319)
- 2026-10-04 — [64a77c9](https://github.com/baiyuqing/otto/commit/64a77c9a901be61cd7f43d89550b827e8d993953): Add scheduled deliveries with prompt runs, recurrence and write-back (#314)
- 2026-10-04 — [4563111](https://github.com/baiyuqing/otto/commit/45631111bcbfa50e88092ed517a29cfad79d5728): Enable natural task guidance and model-scheduled follow-ups (#308)
- 2026-10-04 — [4cb1003](https://github.com/baiyuqing/otto/commit/4cb1003d5f409cd01f383aacbcdb1c195c9b8684): Add model advice to memory candidates before review (#307)
- 2026-10-04 — [8593f0c](https://github.com/baiyuqing/otto/commit/8593f0cf8853eafa3cd78f1fd80ecc3e7beb0abb): Recall approved user preferences on every turn and report pending memory (#306)
- 2026-10-04 — [021e65b](https://github.com/baiyuqing/otto/commit/021e65bf25ca4bd4dba28e0610e4cc7b49e8fa11): Show Telegram command menu buttons on connector startup (#304)
- 2026-10-03 — [59dfe6e](https://github.com/baiyuqing/otto/commit/59dfe6edddb585531eb56fbb4b81bac29991d890): Add direct ACP TCP connections to otto serve (#302)
- 2026-10-03 — [41e12ee](https://github.com/baiyuqing/otto/commit/41e12ee01ccec8f2d1374a3e4ea2306d506f9bbf): Run /compact and /context as ACP slash commands (#301)
- 2026-10-03 — [1a9f7ca](https://github.com/baiyuqing/otto/commit/1a9f7ca92f51e9ed453b15e80bd994c7497cfe2d): Render agent Markdown replies as Telegram message entities (#300)
- 2026-10-03 — [33270bd](https://github.com/baiyuqing/otto/commit/33270bdae6f873f7bb40b2b6d6017e21f120ea45): Set the Telegram command menu to the connector commands (#297)
- 2026-10-03 — [565f8b2](https://github.com/baiyuqing/otto/commit/565f8b23198273bd210bcfa835eff0fb54e4eb9c): Add agent self-help and pending approval withdrawal (#298)
- 2026-10-03 — [d49b2d9](https://github.com/baiyuqing/otto/commit/d49b2d9b211099bb4a84f245116f66aa1dc3d162): Notify local memory review (#292)
- 2026-10-03 — [3dca424](https://github.com/baiyuqing/otto/commit/3dca424f0f0764099179e3b85c02a2dcca66e01b): Replace project logo with the new Otto badge (#290)
- 2026-10-03 — [a64b63d](https://github.com/baiyuqing/otto/commit/a64b63d6ef33aa0e804263ef6f7b6aac67c4246a): Manage Otto profiles through connect (#289)
- 2026-10-02 — [3b63a20](https://github.com/baiyuqing/otto/commit/3b63a200d877c696b21a4e1e6147b72e8258545a): Add Telegram and Feishu cards for memory reviews (#287)
- 2026-10-02 — [8f07f41](https://github.com/baiyuqing/otto/commit/8f07f410186da63c9732638d965c3746e1db2ae5): Add Telegram and Feishu approval buttons (#286)
- 2026-10-02 — [7071066](https://github.com/baiyuqing/otto/commit/7071066647f7db8ae5d781210a1e6abc2963d84b): Make memory review discoverable; page pending and add error codes (#285)
- 2026-10-02 — [6ff7281](https://github.com/baiyuqing/otto/commit/6ff7281f3bad0ff0c79fac664836cd09822738e1): Review memory candidates from the chat connector (#284)
- 2026-10-02 — [c0fd7cf](https://github.com/baiyuqing/otto/commit/c0fd7cf9cfd2c81812ef872afd7a970c81efe6e1): Add the Feishu adapter to otto-connect (#280)
- 2026-10-02 — [37771de](https://github.com/baiyuqing/otto/commit/37771deb1a6b53a81fc1824ab9fd0ca232ec6711): Add otto --attach: the TUI as a client of otto serve (#279)
- 2026-10-02 — [4d690bb](https://github.com/baiyuqing/otto/commit/4d690bbab5a6f98c992e188e9aea1a6ea3dadde5): Expose reflection over HTTP and the Web UI (#278)
- 2026-10-02 — [be6b60f](https://github.com/baiyuqing/otto/commit/be6b60fc5e87801a33988b83112e3c3188735c29): Reflect automatically after a compaction or on exit (#277)
- 2026-10-02 — [12f16a9](https://github.com/baiyuqing/otto/commit/12f16a9f158a509e33dd16ab6d8b9d18d9fa6076): Add otto acp --attach and session commands in otto-connect (#276)
- 2026-10-02 — [95d923f](https://github.com/baiyuqing/otto/commit/95d923f5155feac45fd68e7fb92a7fd720b4ed66): Add session reflection to extract memories and skills from work (#275)
- 2026-10-02 — [f8b4134](https://github.com/baiyuqing/otto/commit/f8b41346ec89898e32aba5fcb2a533cc3ed82960): Update Otto logo (#274)
- 2026-10-02 — [b6f705a](https://github.com/baiyuqing/otto/commit/b6f705adaf8ddf98b80e868d5b092bc3ff1666a5): Share one serve session across clients (#273)
- 2026-10-02 — [6de063d](https://github.com/baiyuqing/otto/commit/6de063d442fd78ca6764b2f1df78fa77dd8dddbd): Add otto-connect, a Go chat connector for Telegram over ACP (#272)
- 2026-10-02 — [947e4b3](https://github.com/baiyuqing/otto/commit/947e4b317a57a27bc7085c5b151a857ec5d234bd): Add otto acp, an Agent Client Protocol v1 agent server (#262)
- 2026-10-02 — [836d06c](https://github.com/baiyuqing/otto/commit/836d06c67cb7d50ddfe703a6c070402875096e5a): Replay read-only tool calls after a session takeover (#261)
- 2026-10-01 — [123c0c6](https://github.com/baiyuqing/otto/commit/123c0c68a362b5b29a528205584b3a3f0ca96740): Report sandbox denials in failed bash results (#260)
- 2026-10-01 — [9f6e2c1](https://github.com/baiyuqing/otto/commit/9f6e2c1f50f5d8c00eb6eadfb2a55bfbfd0c7bab): Reconfigure the Seatbelt sandbox in place on reload (#258)
- 2026-10-01 — [b14c8e8](https://github.com/baiyuqing/otto/commit/b14c8e89fefb17fa688b0bd56b723cbbd0f6919e): Add sandbox excluded_commands (#257)
- 2026-10-01 — [6128097](https://github.com/baiyuqing/otto/commit/6128097ad3d509b75521f90287279d05db1e3988): Install bundled skills and require user closure (#255)
- 2026-10-01 — [5fa1f29](https://github.com/baiyuqing/otto/commit/5fa1f2969dba7b10c960a46dccb542e5bb4a8578): Add sandbox setup skill (#254)
- 2026-10-01 — [bdac8db](https://github.com/baiyuqing/otto/commit/bdac8db93d5f87d83d7ec715c17ca529fc01855a): Redesign the elevated-Bash approval prompt (#253)
- 2026-10-01 — [425f45a](https://github.com/baiyuqing/otto/commit/425f45a7dd1429a8a9acb918d780b5429d266d98): Unify skill management commands (#252)
- 2026-10-01 — [76941ee](https://github.com/baiyuqing/otto/commit/76941ee673dcb1258af0a2534ec1832244525293): Add Ctrl+A and Ctrl+E to the TUI composer (#251)
- 2026-10-01 — [5631bd9](https://github.com/baiyuqing/otto/commit/5631bd917f004bc3af6e696157fdd125b02c2267): Use first prompt for resume session labels (#248)
- 2026-09-30 — [b1c94e9](https://github.com/baiyuqing/otto/commit/b1c94e98b2f8963b76289c93d09400ef807971f7): Add paginated memory management commands (#245)
- 2026-09-30 — [8395f32](https://github.com/baiyuqing/otto/commit/8395f32996e07e7528b8a06c1aaf1d429159d992): Structure system prompt as XML (#243)
- 2026-09-30 — [e2ac5a9](https://github.com/baiyuqing/otto/commit/e2ac5a99bbb4866ba40111b30a465f9ab9e13763): Add first-run setup onboarding (#242)
- 2026-09-30 — [53c0f78](https://github.com/baiyuqing/otto/commit/53c0f786f4557065cc0d506c95f0863ae3409765): Support Pi context edit entries (#241)
- 2026-09-29 — [2f3fc5c](https://github.com/baiyuqing/otto/commit/2f3fc5c3e1ea8c051beb1fa3e72ce68edb616e1f): Shorten TUI status bar (#239)
- 2026-09-29 — [4fd54bd](https://github.com/baiyuqing/otto/commit/4fd54bdc411462ac9a73f9418637fb1250aa796f): Process queued input within active turns (#235)
- 2026-09-29 — [6fc213c](https://github.com/baiyuqing/otto/commit/6fc213c5effcde471401242f71ade6de9487cb5e): Add deadline and cancellation hierarchy (#234)
- 2026-09-28 — [52ed2a0](https://github.com/baiyuqing/otto/commit/52ed2a0e2a4ecf0b867e2f6e9314c36590286882): Refine TUI composer and sub-agent panel (#232)
- 2026-09-28 — [2bcd02c](https://github.com/baiyuqing/otto/commit/2bcd02c8382ca67ca37d9b9a264d5192c8162426): Record resilient tool operation outcomes (#228)
- 2026-09-28 — [17ff557](https://github.com/baiyuqing/otto/commit/17ff557e8d6d6cdbc770970ec9d6883b030a2aa6): Show queued and running sub-agents in a panel below the TUI composer (#230)
- 2026-09-28 — [dee14f1](https://github.com/baiyuqing/otto/commit/dee14f191e67528506e95c0277b72fe62949ac6f): Limit sub-agent agent_report calls to answers and blockers (#227)
- 2026-09-28 — [7551fd9](https://github.com/baiyuqing/otto/commit/7551fd986db6812ec7ad4f95ab03bd54098db9c1): Open /agents during a running turn (#226)
- 2026-09-28 — [bddfdba](https://github.com/baiyuqing/otto/commit/bddfdba8da58b0b22f5e57787f36659cd1dbd73a): Move a lease-managed session to the next host on SIGTERM (#223)
- 2026-09-28 — [137bda5](https://github.com/baiyuqing/otto/commit/137bda596eca0d110ea318cf9bbe5a31088bb714): Add session lease failover, agent resume, and takeover recovery (#222)
- 2026-09-28 — [5fd738c](https://github.com/baiyuqing/otto/commit/5fd738cda83a6734c5b1640e30fb5d67a7b5c504): Make notifications durable across restarts (#221)
- 2026-09-28 — [b965a94](https://github.com/baiyuqing/otto/commit/b965a9439ce9f7731ededddb74b75f8e136fc947): Add /init slash command (#220)
- 2026-09-27 — [523bd0e](https://github.com/baiyuqing/otto/commit/523bd0ea62a83aacc98a4a336ba5548071759562): Show TUI queued input in transcript (#219)
- 2026-09-27 — [2ff979d](https://github.com/baiyuqing/otto/commit/2ff979dedf6880adf1beb52171f5948bcdcfae02): Show queued input in transcript (#218)
- 2026-09-27 — [f67934c](https://github.com/baiyuqing/otto/commit/f67934c0049343d0edb260cbfa12d5983a57b2ab): Improve queued input UI (#216)
- 2026-09-27 — [a6e62cb](https://github.com/baiyuqing/otto/commit/a6e62cbf46ee4373c77975f0f8c418d4e0ed2473): Restore TUI startup logo (#214)
- 2026-09-27 — [4ce4101](https://github.com/baiyuqing/otto/commit/4ce41014e2d9d9aff3579b5093a2f3a0395a89ce): Improve TUI bash approval dialog (#212)
- 2026-09-27 — [a70fd85](https://github.com/baiyuqing/otto/commit/a70fd850a951af34c10f122d1d6d0043b44202e6): Add list_models and stop asking for model ids from memory (#213)
- 2026-09-27 — [666c913](https://github.com/baiyuqing/otto/commit/666c913e32a0b7a1533e839a535e0aee181b44e4): Edit config.toml in place instead of re-serializing it (#211)
- 2026-09-27 — [c579fbd](https://github.com/baiyuqing/otto/commit/c579fbd3373e99ec1ff73e181d715a9b0bfc4f2e): Read active child transcripts without a session lock (#209)
- 2026-09-27 — [7176a57](https://github.com/baiyuqing/otto/commit/7176a57da377567072314b0a8398c72b8e84a9b7): Show bash approval lines in the REPL (#207)
- 2026-09-27 — [e7198b4](https://github.com/baiyuqing/otto/commit/e7198b4e9ac9a824c3558bdc962969227166b861): Carry user messages and require observations in compaction summaries (#206)
- 2026-09-27 — [28b78e7](https://github.com/baiyuqing/otto/commit/28b78e74c11c2e807c749d78585f9ccf71a51b3c): Improve bash approval prompts (#205)
- 2026-09-27 — [9072347](https://github.com/baiyuqing/otto/commit/9072347876276fb98bea8e73dcda0a15320b17c1): Run Open Folder from the desktop Add workspace button (#204)
- 2026-09-27 — [326cbe3](https://github.com/baiyuqing/otto/commit/326cbe3005acdbe7676463d746b2fe2ccc1a6b45): Pick a workspace folder instead of typing its path; otter app icon (#203)
- 2026-09-27 — [4fd71b5](https://github.com/baiyuqing/otto/commit/4fd71b5f9a680886bf87245fc1f71329a98be7a6): Add a macOS desktop shell around otto serve (#201)
- 2026-09-27 — [630e8f3](https://github.com/baiyuqing/otto/commit/630e8f3b55936441ffcab6a855ee140d7e0d9b9d): Add otto trust and otto serve --exit-on-stdin-close (#200)
- 2026-09-27 — [c0b41ec](https://github.com/baiyuqing/otto/commit/c0b41ec7a4ccb302bc3d1b06b86ae122205da447): Remove a workspace, stop status reconnects on 401, refresh Changes after a turn
- 2026-09-26 — [257911b](https://github.com/baiyuqing/otto/commit/257911b259b80c61527c01d8c0a0276ed687198a): Show a working directory's git changes in the Web UI (#198)
- 2026-09-26 — [c956c85](https://github.com/baiyuqing/otto/commit/c956c85718b69a905a46e41745730236f311ea48): Stream process-wide session status and show it in the Web UI sidebar (#197)
- 2026-09-26 — [7d94153](https://github.com/baiyuqing/otto/commit/7d941534966f79e75aa1da173d94ff1d6e23929f): Group Web UI sessions by working directory and persist added workspaces (#196)
- 2026-09-26 — [9ffd4d9](https://github.com/baiyuqing/otto/commit/9ffd4d9070097b173c8f03ff2bae52d5a68db8b6): Serve several workspaces from one otto serve process (#195)
- 2026-09-26 — [32af8b8](https://github.com/baiyuqing/otto/commit/32af8b8955de04a11ea96ff55845a2e112533f67): Improve agent detail close interaction (#193)
- 2026-09-26 — [ae0c9e1](https://github.com/baiyuqing/otto/commit/ae0c9e1c3c3ce7da0a91b6c51fcad99fa182e2d5): Add agent_send for sub-agent updates (#192)
- 2026-09-26 — [7caa552](https://github.com/baiyuqing/otto/commit/7caa552fd45d15446814dda31cabbf3ca17e9458): Check a skill's contract on its first delegation (experimental) (#191)
- 2026-09-25 — [c731200](https://github.com/baiyuqing/otto/commit/c731200a80db7002cd72d7a3d1486b8158cb175e): Record sub-agent tasks and list them across sessions
- 2026-09-25 — [2fd24ad](https://github.com/baiyuqing/otto/commit/2fd24ad6c9ed7aa3ea724506886a7fdec928e4d1): Add queued TUI input while turns run (#189)
- 2026-09-25 — [59ebe96](https://github.com/baiyuqing/otto/commit/59ebe96ba77e9407c5c0c5cb1c9bbd9a5a577418): Modernize web rename dialog (#188)
- 2026-09-25 — [479cc03](https://github.com/baiyuqing/otto/commit/479cc03ee98b736443b2b24dec957737d8614dcb): Show agent reasoning, turn phase, sub-agent transcripts, and session context (#186)
- 2026-09-25 — [386f7b9](https://github.com/baiyuqing/otto/commit/386f7b92ca067d44c3bfe643b74e0136da17ded8): Skip gitignored files in grep and find (#185)
- 2026-09-22 — [51a52cf](https://github.com/baiyuqing/otto/commit/51a52cfb987a5db1c5f1e2120ed8e1aa1a842a91): Run a contracted skill as a sub-agent (#180)
- 2026-09-22 — [e02d3bf](https://github.com/baiyuqing/otto/commit/e02d3bf54ab8b253ec42372876ef851d19651099): Read an optional input/output contract from a skill's frontmatter (#179)
- 2026-09-22 — [acbc5a8](https://github.com/baiyuqing/otto/commit/acbc5a845aa50c1e8d173a86221b9e9b43249c6f): Keep every skill listed when the prompt section overflows, and draft sub-agent execution (#178)
- 2026-09-22 — [86317ab](https://github.com/baiyuqing/otto/commit/86317abf16c0ec5ad5875c33d1483a1487fcbeab): Tui: band a prompt so a turn boundary is findable (#176)
- 2026-09-22 — [2f956ee](https://github.com/baiyuqing/otto/commit/2f956ee1a5a54a8ab365fea0f3789bb4c5702608): Use one prompt marker, ❯, in both terminal frontends (#175)
- 2026-09-22 — [193a528](https://github.com/baiyuqing/otto/commit/193a5284345194b8109ba6c9f8882252c916a690): Tui: give the transcript a gutter so a turn is readable at a glance (#172)
- 2026-09-22 — [ad06310](https://github.com/baiyuqing/otto/commit/ad063101c4b54a62f4f01811ee5132c6934c8b1d): Cli: break down runner startup trace (#167)
- 2026-09-22 — [963af18](https://github.com/baiyuqing/otto/commit/963af18ad295ba42b05b8296cedc9a0f8dc10574): Cli: add startup timing trace (#165)
- 2026-09-21 — [ab51961](https://github.com/baiyuqing/otto/commit/ab51961a8c57d1fdf554e2d26c266fc3342a78a1): Add workflow time travel (#162)
- 2026-09-21 — [5534225](https://github.com/baiyuqing/otto/commit/553422588449deb8197df1bdc6305fb962537c55): Add workflow write coordination policies (#161)
- 2026-09-21 — [c92ac52](https://github.com/baiyuqing/otto/commit/c92ac52bbfa6245bbb931c3014869995100e847a): Add workflow handoff (#160)
- 2026-09-21 — [1fa253d](https://github.com/baiyuqing/otto/commit/1fa253ddb2b15d08b74486a47e5b0b31dbdf3750): Add durable workflow runtime (#159)
- 2026-09-21 — [d7b3408](https://github.com/baiyuqing/otto/commit/d7b34088d24a0ac893384b488a8443e2ceac7ee0): Replace project logo (#157)
- 2026-09-21 — [d941c31](https://github.com/baiyuqing/otto/commit/d941c3140cf2957c01d10d419f46eaeae652df4c): Auto-grow web composer textarea (#156)
- 2026-09-21 — [40df647](https://github.com/baiyuqing/otto/commit/40df6477ad6696f4c22848cbb14464194ed024eb): Name the session workspace in the TUI picker rows (#154)
- 2026-09-21 — [14eda57](https://github.com/baiyuqing/otto/commit/14eda57ffe5f6d28548b74f7cf1394e052028030): Back up and compare-and-swap every config file write (#152)
- 2026-09-21 — [dc80b64](https://github.com/baiyuqing/otto/commit/dc80b64565ad3a0e2f6993c36eaa6421aa67a8d5): Grant sandbox read paths and network from a running session (#151)
- 2026-09-21 — [1d2bf48](https://github.com/baiyuqing/otto/commit/1d2bf48d66a3ff95f0cf255b701b852ca79e5782): Show full sandbox status in TUI footer (#148)
- 2026-09-21 — [fca19b5](https://github.com/baiyuqing/otto/commit/fca19b56147a6344d207b8a30e9ee06ffe25182d): Add thinking level picker (#147)
- 2026-09-21 — [40add03](https://github.com/baiyuqing/otto/commit/40add03da703c390dd9d6ef4e140649c2197b76e): Persist session thinking level (#146)
- 2026-09-20 — [258d0f4](https://github.com/baiyuqing/otto/commit/258d0f435f5314705a802fb10f6c814ba8ee9a46): Guide the model's sub-agent context choice (#144)
- 2026-09-20 — [3a271cc](https://github.com/baiyuqing/otto/commit/3a271cc8a680ce65d66944f2f2a5c267f0966795): List and cancel session timers (#143)
- 2026-09-20 — [df9f192](https://github.com/baiyuqing/otto/commit/df9f192c767533ca69fb792d3dacc532234fab0d): Keep /model switches in the current session (#142)
- 2026-09-20 — [220d5c7](https://github.com/baiyuqing/otto/commit/220d5c7b11096ec3a9d8f19d54a0ade981e2211c): Keep current model settings for new sessions (#141)
- 2026-09-20 — [86697dc](https://github.com/baiyuqing/otto/commit/86697dcd968739781ec3c9b4fe7bc33ffda927ec): Add MCP config management commands (#140)
- 2026-09-20 — [2188120](https://github.com/baiyuqing/otto/commit/21881202211337240d6c4a8e8549fdceba56bd9d): Draw the TUI text selection in-app (#139)
- 2026-09-20 — [a4f6b93](https://github.com/baiyuqing/otto/commit/a4f6b9379695965c6902b7027cff365d24e4facd): Move the TUI transcript into the terminal's scrollback (#137)
- 2026-09-20 — [e83bf2b](https://github.com/baiyuqing/otto/commit/e83bf2b6ab6665e05f60f225afe6f3db3ed20ac5): Improve TUI mouse selection behavior (#136)
- 2026-09-20 — [23413c9](https://github.com/baiyuqing/otto/commit/23413c9fa98a38f2007807a42b6fdedba66d71d5): Show timestamps and tool details in web UI (#135)
- 2026-09-20 — [22146d2](https://github.com/baiyuqing/otto/commit/22146d2819ce6773bbbac1086dd89baf4b6e5841): Add side margins to TUI layout (#133)
- 2026-09-20 — [6cb2041](https://github.com/baiyuqing/otto/commit/6cb2041a4cd3a0c0855ec5a05b16271a72182922): Make reasoning effort configurable (#132)
- 2026-09-19 — [73656b7](https://github.com/baiyuqing/otto/commit/73656b770efb9a1ec099839489726d2fabca9fbb): Add MCP client: stdio and Streamable HTTP tool servers with OAuth 2.1 (#129)
- 2026-09-19 — [f641ece](https://github.com/baiyuqing/otto/commit/f641ece645cbb17889358f5a3b100a878ec94dd7): Use the otter logo as the web page icon (#128)
- 2026-09-19 — [a7e242d](https://github.com/baiyuqing/otto/commit/a7e242d56cd0bc3e3c7cbff8e97c2989230764b4): Persist token usage history (#127)
- 2026-09-19 — [48f9271](https://github.com/baiyuqing/otto/commit/48f9271484aa547e72a160063b5930bc4679cab0): Show sent images in the web transcript (#125)
- 2026-09-19 — [ac1e15c](https://github.com/baiyuqing/otto/commit/ac1e15cc7f6a30c5d87ab32dcab5c698e4359cf6): Add image inputs to TUI and web (#123)
- 2026-09-17 — [31832f8](https://github.com/baiyuqing/otto/commit/31832f8c0023c5a67b5458ff3f073a3919d91f2d): Open the serve URL in the default browser (#119)
- 2026-09-17 — [d2bfa07](https://github.com/baiyuqing/otto/commit/d2bfa07355f5e1aa9e148b80e8dfb36236189b37): Expand Feishu merge_forward via lark-cli mget (#118)
- 2026-09-16 — [718aaed](https://github.com/baiyuqing/otto/commit/718aaedfedf12f5d36908e84c5ac7dd857967274): Deliver Feishu messages into the inbox, and stop calling Otto a coding agent (#115)
- 2026-09-15 — [bc135fe](https://github.com/baiyuqing/otto/commit/bc135fe87674e3f97b69eae90a5e830a1459869e): Persist remind timers with the session (#114)
- 2026-09-15 — [28d328c](https://github.com/baiyuqing/otto/commit/28d328cd8aca1ba8706196d94ec31ad52e0b7f58): Add an in-process remind timer (#113)
- 2026-09-15 — [3648460](https://github.com/baiyuqing/otto/commit/364846096b28a558d5c578346f154079ff73b697): Wake the idle transcript when a sub-agent finishes (#112)
- 2026-09-15 — [6c93a30](https://github.com/baiyuqing/otto/commit/6c93a3080554545dac7702d4fc2fc971bba64dce): Add one-shot sandbox approvals (#111)
- 2026-09-15 — [d708f2a](https://github.com/baiyuqing/otto/commit/d708f2a1cdb77963b9f1f728690626486a59318d): Add user-facing skill commands (#110)
- 2026-09-14 — [8202807](https://github.com/baiyuqing/otto/commit/82028077f2a9d4f26761d0227b1cc9f8b3406c1e): Recall prompts with the arrow keys and fold tool calls (#104)
- 2026-09-14 — [3877106](https://github.com/baiyuqing/otto/commit/38771069d0612cfb250a5fde32a4e31ac95db786): Distinguish user messages and show a thinking indicator (#102)
- 2026-09-13 — [f1e433f](https://github.com/baiyuqing/otto/commit/f1e433fe12f30811f3fbe5573f03abc3aa32e575): Support robust edit replacements (#90)
- 2026-09-06 — [f4ecec4](https://github.com/baiyuqing/otto/commit/f4ecec453ef06457d802f85b27639581d8a00065): Render math and mermaid in web markdown (#89)
- 2026-09-06 — [922c64b](https://github.com/baiyuqing/otto/commit/922c64b152160dd15dd3a9fb5c72522ac15f899b): Handle web slash commands (#88)
- 2026-09-06 — [97e5361](https://github.com/baiyuqing/otto/commit/97e5361a07c6494b7036b317f92016ed5c857aad): Suggest web slash commands (#87)
- 2026-09-06 — [9473da8](https://github.com/baiyuqing/otto/commit/9473da8f4f9b39537e449ad79b41a5b982318af4): Handle rename command in web ui (#86)
- 2026-09-06 — [fcfe28d](https://github.com/baiyuqing/otto/commit/fcfe28d28cd0d95d3264a65fe114fd33eada2e49): Support session rename (#85)
- 2026-09-06 — [50a5b4d](https://github.com/baiyuqing/otto/commit/50a5b4d48837811b53e24bc23623f7e185182929): Polish web ui (#84)
- 2026-09-06 — [ec347c7](https://github.com/baiyuqing/otto/commit/ec347c7878bb9722aaf01c13a71868e2b24f6d03): Browser web UI served by otto serve over loopback TCP (#81)
- 2026-09-06 — [c22182d](https://github.com/baiyuqing/otto/commit/c22182dd7b465d257447ee95e061a706316afc91): Record full request and response bodies in OTTO_TRACE (#80)
- 2026-09-06 — [14fc7e0](https://github.com/baiyuqing/otto/commit/14fc7e01a7b067bb7afbd0675244e58a9bb647da): Reload the [sandbox] configuration without restarting (#79)
- 2026-09-06 — [f09d1d3](https://github.com/baiyuqing/otto/commit/f09d1d392ec468d1e024a315c564b9de89d5d902): Add provider and context metrics (#78)
- 2026-09-05 — [d2ca35d](https://github.com/baiyuqing/otto/commit/d2ca35dc1cb1d0259f2756ffa0530a47d8658044): Add interactive sandbox setup (#69)
- 2026-09-04 — [779b8ae](https://github.com/baiyuqing/otto/commit/779b8ae0070c286d2995a8936a6dbb98e5639b96): Named sub-agent definitions, [agents] config, and context inherit (#61)
- 2026-09-04 — [e3f4149](https://github.com/baiyuqing/otto/commit/e3f4149e7e5e3eaf8128e884d979ec4cf8b9b480): Task panel, wake turns, and /tasks commands for sub-agents (#60)
- 2026-09-04 — [6795f30](https://github.com/baiyuqing/otto/commit/6795f302b47d2a8ee986b571c714090e3727bf24): Wake turns, task routes, and task metrics for sub-agents (#59)
- 2026-09-04 — [46b7afe](https://github.com/baiyuqing/otto/commit/46b7afef598d050840ecb90c1fdaac20cb666869): Let each agent call choose the sub-agent model (#58)
- 2026-09-04 — [4b046e3](https://github.com/baiyuqing/otto/commit/4b046e3783405d671a338fc11ead921c7a289996): Add async sub-agents (#57)
- 2026-09-03 — [6205ed8](https://github.com/baiyuqing/otto/commit/6205ed824fe275a75fc7a11c46d14ee9ea750efb): Render transcript inline into terminal scrollback (#55)
- 2026-09-03 — [eef7b20](https://github.com/baiyuqing/otto/commit/eef7b20aa7dc684652f5a05595fefd2a603ee56f): Add Unix-socket agent server (#54)
- 2026-09-03 — [95bc7f2](https://github.com/baiyuqing/otto/commit/95bc7f2d7d53f0fd109008b64b3001e14d6863c0): Add on-demand skills (Agent Skills format) (#53)
- 2026-09-02 — [15b0f29](https://github.com/baiyuqing/otto/commit/15b0f291ce4d2aa47a77cf31e6f5de2946c57905): Add workspace context to the system prompt (#51)
- 2026-09-02 — [2b70572](https://github.com/baiyuqing/otto/commit/2b7057220e62edfa61c6479a1fb4a8147f64a8f1): Show tool call_id in the trace viewer (#50)
- 2026-09-02 — [8e8e414](https://github.com/baiyuqing/otto/commit/8e8e41400d881af846f556ef3b1bff6a885ddb40): Add OTTO_TRACE provider HTTP wire capture (#45)
- 2026-09-01 — [b9a86c2](https://github.com/baiyuqing/otto/commit/b9a86c2bc78654e848a145bafcf39888ca9c4e2f): Add macOS sandbox driver architecture (#44)
- 2026-09-01 — [ccf553c](https://github.com/baiyuqing/otto/commit/ccf553c3d1574589b51452c94c4f39c456c57188): Persist selected model profile as default (#43)
- 2026-09-01 — [d722951](https://github.com/baiyuqing/otto/commit/d722951dbddd49461d8289615cd74f16510adc0b): Add TUI model profile picker (#41)
- 2026-09-01 — [6aaa99a](https://github.com/baiyuqing/otto/commit/6aaa99acc06e29a36ff324e931bf4127c482d522): ChatGPT subscription authorization (#40)
- 2026-08-31 — [46c07e7](https://github.com/baiyuqing/otto/commit/46c07e70d6488f9298ba5c7b41badba221ffee2b): Restyle tool calls as neutral code blocks (#39)
- 2026-08-31 — [e663be2](https://github.com/baiyuqing/otto/commit/e663be25b57022e175dfc1eb8ed685ac21ad46b7): Redesign tool call display (#38)
- 2026-08-31 — [cd6a427](https://github.com/baiyuqing/otto/commit/cd6a427c32e09f3f3f44458e54ade78b87bd780b): Context usage progress bar in footer (#37)
- 2026-08-31 — [241de93](https://github.com/baiyuqing/otto/commit/241de93bb2ed7cbdae5a9667dcd5255b41dd6c1f): Inline REPL with native text selection (#36)
- 2026-08-31 — [6ead19c](https://github.com/baiyuqing/otto/commit/6ead19c1953cfbc04e76ba0177faccf836955613): Wire Otto memory (recall + explicit management) (#35)
- 2026-08-31 — [44df53f](https://github.com/baiyuqing/otto/commit/44df53f13e5b440770867093763ca85ba678a82e): Non-destructive session archiving (#34)
- 2026-08-30 — [eed483f](https://github.com/baiyuqing/otto/commit/eed483f2223464d98de2aebff4b1492535de32ab): Complete SQLite memory core
- 2026-08-30 — [6010cd4](https://github.com/baiyuqing/otto/commit/6010cd432d224b4e6a07dee3a3abdbe1f20b17f7): Retrieve scoped SQLite memory
- 2026-08-30 — [4fc8270](https://github.com/baiyuqing/otto/commit/4fc8270bcf11210f40269dcf65d96c8ea6995e8d): Persist memory candidates
- 2026-08-30 — [17d4e4d](https://github.com/baiyuqing/otto/commit/17d4e4dc66b2cdd979e16faf58cbf915e04de79d): Enforce memory record revisions
- 2026-08-30 — [efce2e1](https://github.com/baiyuqing/otto/commit/efce2e14c7a8a477bf9659b461b9bafd86ca79c3): Persist SQLite memory records
- 2026-08-30 — [e6aafd0](https://github.com/baiyuqing/otto/commit/e6aafd0be3cd43c77d26b80abe47864cf0840e23): Open SQLite memory store
- 2026-08-30 — [d3807cf](https://github.com/baiyuqing/otto/commit/d3807cf1236b562a3c48713dda4452e393ba4355): Add safe memory policy fallback
- 2026-08-30 — [8752601](https://github.com/baiyuqing/otto/commit/875260135007bbe0496b0f4664b58518409192e7): Validate durable memory content
- 2026-08-30 — [5fac056](https://github.com/baiyuqing/otto/commit/5fac0563d5210543ba6bba25f99f0071f5eb46af): Define extensible memory contracts
- 2026-08-30 — [fc71957](https://github.com/baiyuqing/otto/commit/fc71957d9aa9f13a5ca209758c6cbead7b27d45e): Surface compaction execution plan in TUI
- 2026-08-30 — [e1162da](https://github.com/baiyuqing/otto/commit/e1162dae774d58154b30d9940d25fd1f2eed4f35): Show ASCII logo in TUI empty state
- 2026-08-30 — [6c95e43](https://github.com/baiyuqing/otto/commit/6c95e4376ac72ef0339ada7f1f8c6cd15c8c9117): Add ASCII logo to REPL startup
- 2026-08-30 — [b32e850](https://github.com/baiyuqing/otto/commit/b32e850010aca012614775b49e956fc2a012f97a): Add logo assets
- 2026-08-30 — [eb03d52](https://github.com/baiyuqing/otto/commit/eb03d5286ddec73a38d3d28d43340c1614df3efe): Add (●ᴥ●) otto mark to TUI empty state and REPL startup
- 2026-08-29 — [1e5fe74](https://github.com/baiyuqing/otto/commit/1e5fe7444abe3d91d41d631d06d11da45bc93e2b): Show TUI context usage
- 2026-08-29 — [78709db](https://github.com/baiyuqing/otto/commit/78709dbdfcdeeba5f043134eeeb716cf9e92cd2e): Format TUI status statistics
- 2026-08-29 — [6af25fa](https://github.com/baiyuqing/otto/commit/6af25fa87e7fd3927e78a3a7c952002cc761882f): Frame the composer as the core input panel
- 2026-08-29 — [fb43b99](https://github.com/baiyuqing/otto/commit/fb43b9996296665849041106065b4988353fd21c): Enlarge and elevate the TUI input box
- 2026-08-29 — [d68d4b8](https://github.com/baiyuqing/otto/commit/d68d4b8c5295ab65af7fe49a3c0d081d910435f4): Clarify TUI transcript hierarchy
- 2026-08-29 — [6514902](https://github.com/baiyuqing/otto/commit/651490253a6c1e67eaf0f693475431723ecd1942): Prompt history, turn timer, and empty-state hint
- 2026-08-29 — [fab54ae](https://github.com/baiyuqing/otto/commit/fab54ae0834f28f456e9d1535f2db1ca3c03a528): Render folded compaction checkpoints
- 2026-08-29 — [2412ef4](https://github.com/baiyuqing/otto/commit/2412ef446ad19d1ef5f3c3b48e4b44052a863752): Run compaction from the TUI
- 2026-08-29 — [e5be93c](https://github.com/baiyuqing/otto/commit/e5be93c60db64f77f3b4de9873fc5b4119843b82): Add REPL compact command
- 2026-08-29 — [207a5fe](https://github.com/baiyuqing/otto/commit/207a5fe38eca0dc903461b9e7dc3affe1b62166f): Serialize compaction lifecycle
- 2026-08-29 — [db76e0c](https://github.com/baiyuqing/otto/commit/db76e0c6ca56a1458cf02eba23f3968c977dea82): Compact automatically on context pressure
- 2026-08-29 — [4939bf1](https://github.com/baiyuqing/otto/commit/4939bf16c644e49b5a9ef3d2b6b897de6b169f4d): Add manual context compaction
- 2026-08-29 — [eab3367](https://github.com/baiyuqing/otto/commit/eab33675af24ad409bfdd50cf6fc2ea7674e89db): Append Pi compaction checkpoints
- 2026-08-29 — [8c400fb](https://github.com/baiyuqing/otto/commit/8c400fbad973ff2390ce41742fff801581bfcdc2): Prepare structured compaction summaries
- 2026-08-29 — [c78d020](https://github.com/baiyuqing/otto/commit/c78d020feae2d0ff74f65cee293dd665419c8977): Select safe compaction cuts
- 2026-08-29 — [9ffaad9](https://github.com/baiyuqing/otto/commit/9ffaad97ee680b94243a2970c7511e29cc6fb687): Estimate provider request context
- 2026-08-29 — [793f139](https://github.com/baiyuqing/otto/commit/793f139e46980ccd8a864a1fdcef18eacc266a54): Classify context overflow errors
- 2026-08-29 — [617bdc7](https://github.com/baiyuqing/otto/commit/617bdc7731678da75d11bbf38f4154b31c355fd2): Resolve compaction model limits
- 2026-08-28 — [998f1b9](https://github.com/baiyuqing/otto/commit/998f1b965b7831142e52bd4ebff56200d43604c2): Parse and display provider cached prompt tokens
- 2026-08-28 — [c1f3526](https://github.com/baiyuqing/otto/commit/c1f35265cd135ebffb783f6c12462f271346945f): Add built-in search tools
- 2026-08-28 — [d58423a](https://github.com/baiyuqing/otto/commit/d58423a42db485f64739d6fccbf8a6265c03d11e): Add --approve flag for headless one-shot prompts
- 2026-08-28 — [3a24405](https://github.com/baiyuqing/otto/commit/3a244057605f67804fef683e6e096e858309858f): Add --thinking flag to set model reasoning effort
- 2026-08-28 — [b7efede](https://github.com/baiyuqing/otto/commit/b7efede69a9028381c507201bca305eed66672b7): Return compact diffs and concise errors from edit tool
- 2026-08-28 — [534b4ff](https://github.com/baiyuqing/otto/commit/534b4ff6ecf41bc5f4249b40a1c686ed030ec534): Create session files lazily on first user prompt
- 2026-08-28 — [4d699f5](https://github.com/baiyuqing/otto/commit/4d699f5d26c7608ae0949655816c3cc67103bd99): Fold tool output by default
- 2026-08-28 — [0d377aa](https://github.com/baiyuqing/otto/commit/0d377aa533cde9c05a22470ca093dd2b6131f6d0): Render responsive session picker
- 2026-08-27 — [d7b13e6](https://github.com/baiyuqing/otto/commit/d7b13e69390a4de7260ba3663c19fd327785678f): Add TUI resume lifecycle
- 2026-08-27 — [a67b403](https://github.com/baiyuqing/otto/commit/a67b40379d5c3a536e2f4b19ed9933f606095b8b): Switch sessions transactionally
- 2026-08-27 — [3b15a02](https://github.com/baiyuqing/otto/commit/3b15a02060473d376d73dd322064665c046707d5): List Pi sessions for a workspace
- 2026-08-27 — [36832db](https://github.com/baiyuqing/otto/commit/36832db46c407947fdd572aee08bb251d06c1e83): Build context from Pi session branches
- 2026-08-27 — [b1494ae](https://github.com/baiyuqing/otto/commit/b1494aee7e0372104f571dfcfad01de17e8963ff): Persist sessions as Pi v3 trees
- 2026-08-27 — [2e2f3c8](https://github.com/baiyuqing/otto/commit/2e2f3c85e38136b1a1a86ef595943107fb4deadf): Add Pi v3 session codec
- 2026-08-27 — [a0b9b80](https://github.com/baiyuqing/otto/commit/a0b9b8022af4b9607a54222550fa8b04654d1a7f): Add slash command completion
- 2026-08-27 — [32f1b93](https://github.com/baiyuqing/otto/commit/32f1b93b7e6ee7eddab7a84b600480349287a307): Select adaptive terminal UI
- 2026-08-27 — [dc71b12](https://github.com/baiyuqing/otto/commit/dc71b1293812bad883d5b8d3027a43d482f13eec): Add TUI commands and keybindings
- 2026-08-27 — [84d96af](https://github.com/baiyuqing/otto/commit/84d96afa201ed91a2c08b7b476c42973c2433705): Stream agent turns in TUI
- 2026-08-27 — [ce2f795](https://github.com/baiyuqing/otto/commit/ce2f795246d94397187a8b8f2a313e3bc3021af9): Add full-screen TUI layout
- 2026-08-27 — [c2ddaa7](https://github.com/baiyuqing/otto/commit/c2ddaa7e0e32bffdf2320052db2912207edd60b9): Add TUI transcript rendering
- 2026-08-27 — [83aedb4](https://github.com/baiyuqing/otto/commit/83aedb481f5d5d8fa25d367713ec59f1eaac6c14): Add UI mode configuration
- 2026-08-26 — [d1212b8](https://github.com/baiyuqing/otto/commit/d1212b8c63b0651c315881cff7aa8e22eb86779d): Add Otto command-line REPL
- 2026-08-26 — [268c5c3](https://github.com/baiyuqing/otto/commit/268c5c311aa9becd4ff8d8e7e75689006ff825e2): Add durable agent tool loop
- 2026-08-26 — [ea4f6c2](https://github.com/baiyuqing/otto/commit/ea4f6c2d52fa131c66bbfedf4ccc05376f9d2da0): Add OpenAI-compatible streaming
- 2026-08-26 — [f7b37ff](https://github.com/baiyuqing/otto/commit/f7b37ff0ac71b91e6dac65283e5840572d7c92d8): Add cancellable shell tool
- 2026-08-26 — [1fbdc51](https://github.com/baiyuqing/otto/commit/1fbdc51dcee1b112e1e1b2b55492d909d0c14293): Add workspace file tools
- 2026-08-26 — [7a89e20](https://github.com/baiyuqing/otto/commit/7a89e2088721ec934859ff27a524ea7f8bd7fd5e): Add resumable JSONL sessions
- 2026-08-26 — [c65f893](https://github.com/baiyuqing/otto/commit/c65f893240dc4cbad29d894c237e36bb4bd9ac23): Add TOML provider profiles

</details>

<details>
<summary>Fixes (135 commits)</summary>

- 2026-10-05 — [c7c8b65](https://github.com/baiyuqing/otto/commit/c7c8b656360576b1d0bf2b3d6610f9d5141561f2): Fix edit panic on a BOM-only old_text and loose trailing-whitespace matches (#329)
- 2026-10-04 — [096dd20](https://github.com/baiyuqing/otto/commit/096dd20791e6c16eac5690a99c0f7eac34a00fac): Stop `log stream` from outliving otto (#325)
- 2026-10-04 — [70f672d](https://github.com/baiyuqing/otto/commit/70f672dd2099a8f51aba9a75ad96bf8cf0d78fe3): Fix escaped characters in tool argument displays (#320)
- 2026-10-04 — [cc1e1db](https://github.com/baiyuqing/otto/commit/cc1e1db6b40d2da997f5c36a604e02e10241ea79): Fix schedule blank alternate field (#317)
- 2026-10-04 — [a503692](https://github.com/baiyuqing/otto/commit/a5036921965035515a3231052bc3388f98e3c098): Admit an ACP prompt when the wake turn ends during admission (#315)
- 2026-10-04 — [fdd798a](https://github.com/baiyuqing/otto/commit/fdd798a55fc9173023199b59764dad89dd8a22e9): Fix ACP prompt completion admission race (#313)
- 2026-10-04 — [e8b58a9](https://github.com/baiyuqing/otto/commit/e8b58a97e4c982723107354d0560dac2c1eb31f4): Send wake-turn replies to ACP clients and chats (#312)
- 2026-10-04 — [6b5a1dd](https://github.com/baiyuqing/otto/commit/6b5a1ddbf5d9934e9d0c04d279696fd4ff7b9b92): Fix memory deletion proposals with exact record lookup (#310)
- 2026-10-03 — [f6a711e](https://github.com/baiyuqing/otto/commit/f6a711ed0d9773435b5082098d00ac70554a561d): Retry ChatGPT transport failures with visible progress (#293)
- 2026-10-02 — [36e8f46](https://github.com/baiyuqing/otto/commit/36e8f46c03aa8471cfbefed0cbacce300c146cda): Lower unhandled Feishu read receipt logs to debug (#283)
- 2026-10-01 — [135e5ea](https://github.com/baiyuqing/otto/commit/135e5ea3baa2ed1334c07cafabe74701de59e5ba): Fix TUI input history during active turns (#256)
- 2026-10-01 — [c0a94df](https://github.com/baiyuqing/otto/commit/c0a94df92fc46f36cf8f8e37abfcb6989a06c181): Prevent warnings from corrupting the TUI (#249)
- 2026-09-29 — [152bd91](https://github.com/baiyuqing/otto/commit/152bd911898b57818ddbaab623208a21fcf8b9b1): Retry session locks through a concurrent spawn (#238)
- 2026-09-28 — [810664f](https://github.com/baiyuqing/otto/commit/810664f1bbe00f3b9e5309157598b3a390ae8b51): Remove the serve socket file at shutdown (#224)
- 2026-09-27 — [f315910](https://github.com/baiyuqing/otto/commit/f3159103b57510e71644e87e5e3b4dd93688e972): Avoid inherited session locks (#217)
- 2026-09-27 — [00b5078](https://github.com/baiyuqing/otto/commit/00b5078ed446870e35f95edc317a64f00c5f3758): Serialize config file writes (#215)
- 2026-09-27 — [8ac3f18](https://github.com/baiyuqing/otto/commit/8ac3f18d6dcf8418ad05b1aefbc3afe6bcc77c25): Prevent archiving active sessions (#210)
- 2026-09-27 — [1a057c4](https://github.com/baiyuqing/otto/commit/1a057c4f6dfba1dc985e869172f13d887e8f5e8e): Prevent concurrent session file writers (#208)
- 2026-09-23 — [e8e79a6](https://github.com/baiyuqing/otto/commit/e8e79a6d91ac6169e12fab1a737660918dfc713f): Fix edit ambiguity, fuzzy whitespace loss, and lock aliasing (#184)
- 2026-09-23 — [133b23a](https://github.com/baiyuqing/otto/commit/133b23a3081b3f657db55c52d75351e05ccecaf5): Release the tool calls Otto gives up on (#183)
- 2026-09-22 — [d3faa9e](https://github.com/baiyuqing/otto/commit/d3faa9e649c326f309929a0739dd738b609f02e0): Session: collapse resume-picker rows that share a session id (#170)
- 2026-09-22 — [0833e9c](https://github.com/baiyuqing/otto/commit/0833e9cf77fffd55f76f8e1226e1f447c7f25b99): Cli: report startup trace before frontend (#166)
- 2026-09-21 — [c8f7d75](https://github.com/baiyuqing/otto/commit/c8f7d7519ff251ab15a4fc9f2de35352661bef82): Fix TUI PTY bracketed paste parsing (#158)
- 2026-09-21 — [f1df643](https://github.com/baiyuqing/otto/commit/f1df643679d6937b831058a7b10034b040e6ceed): Fix the two warnings a session in $HOME prints at startup (#155)
- 2026-09-20 — [75fdd71](https://github.com/baiyuqing/otto/commit/75fdd7108acfad303f2faa909f8ed14bccbe164f): Normalize raw turn text into a recall query (#134)
- 2026-09-20 — [ca05efb](https://github.com/baiyuqing/otto/commit/ca05efbf29c71379b58ba838da5276e3fe344358): Make /clear start a fresh session (#131)
- 2026-09-19 — [49de022](https://github.com/baiyuqing/otto/commit/49de022879dfdda6bfb22366f04603ae126db6d0): Address review findings on the MCP client (#130)
- 2026-09-19 — [9a9fbae](https://github.com/baiyuqing/otto/commit/9a9fbae7f67b995229fa4c187cb4fe5f8e9f66d1): Make image paste the primary path (#124)
- 2026-09-17 — [d6e0f1e](https://github.com/baiyuqing/otto/commit/d6e0f1ec2ddc3d539bbc6bcac566d9e619a7347d): Restyle the Web UI so code stays readable (#120)
- 2026-09-17 — [e55946c](https://github.com/baiyuqing/otto/commit/e55946cb9d4ea01d65be706efa8a18348e46a9da): Follow idle-page wake turns without a reload (#116)
- 2026-09-15 — [2f18f4b](https://github.com/baiyuqing/otto/commit/2f18f4b327235707fda20e4c4b99d3ee838835fd): Read a blank old_text/new_text pair beside edits as absent (#109)
- 2026-09-15 — [1fcc779](https://github.com/baiyuqing/otto/commit/1fcc779baca821499e4dbaf04a6c5c9d84cea24c): Treat an empty edits array as an absent key (#107)
- 2026-09-14 — [bf6d921](https://github.com/baiyuqing/otto/commit/bf6d921e3bcda8365ba7c042b0ebd8dff7faf40d): Answer a bare /task with its usage (#106)
- 2026-09-14 — [bf98eee](https://github.com/baiyuqing/otto/commit/bf98eee86d193c5c1329f779851a4ddda989cb95): Let the transcript scroll while a turn is running (#103)
- 2026-09-14 — [0a0b46f](https://github.com/baiyuqing/otto/commit/0a0b46f55b194738e22bd3230a6b7e158346b259): Show the submitted prompt in the transcript (#101)
- 2026-09-14 — [e34a73f](https://github.com/baiyuqing/otto/commit/e34a73f41236c311dce74a7eee629c122fc760f7): Place the composer caret on the grid the text renders on (#100)
- 2026-09-14 — [c18bd48](https://github.com/baiyuqing/otto/commit/c18bd48a95fcf45f77c7512a24c7711b3402dd17): Select slash-command suggestions with the arrow keys (#99)
- 2026-09-14 — [75cfb71](https://github.com/baiyuqing/otto/commit/75cfb718be0756377e73671e209fdcb164aeb5c0): List slash commands while typing (#98)
- 2026-09-14 — [9a0454f](https://github.com/baiyuqing/otto/commit/9a0454f60e0cec85ebcb58bc4073fc18e7f5c297): Clear TUI picker background (#96)
- 2026-09-14 — [7a3e6d0](https://github.com/baiyuqing/otto/commit/7a3e6d06d8cd0516d782fa8eebbe48be9b2b7c2a): Restore the TUI status bar (#95)
- 2026-09-14 — [0d3c22a](https://github.com/baiyuqing/otto/commit/0d3c22a90b7aaee59924ee0d63ef3f7be98005b0): Fix TUI mouse-wheel scrolling (#94)
- 2026-09-13 — [6f6a435](https://github.com/baiyuqing/otto/commit/6f6a435f2e8f70189f5dea545c553febabe11005): Address review findings for edit replacements (#91)
- 2026-09-06 — [2cb7af7](https://github.com/baiyuqing/otto/commit/2cb7af7b7b7bfddce589a43ed2c637f5da9df23c): Single-row composer with the shortcut hint below the box (#83)
- 2026-09-06 — [f000ce7](https://github.com/baiyuqing/otto/commit/f000ce75d3a04ce8b2a63d20471e8b5e825540c0): Split scrollback prints to fit above the live frame (#77)
- 2026-09-05 — [ca6158e](https://github.com/baiyuqing/otto/commit/ca6158eba3859e8bc5ba653a5f1035579d7a965a): Allow /etc traversal and the trustd broker so TLS clients work (#76)
- 2026-09-05 — [3bf0de8](https://github.com/baiyuqing/otto/commit/3bf0de848dda67deaa776e4935a10b18ba4b61b0): Close PTY cleanly after tui.Run exits, not before (#75)
- 2026-09-05 — [1fe0b1d](https://github.com/baiyuqing/otto/commit/1fe0b1d31bcea2e240dd8c1c961e77b35a8bab1f): Preserve JSON schema number precision (#73)
- 2026-09-05 — [c310035](https://github.com/baiyuqing/otto/commit/c3100351e81522529f85316bcb03b4d1a196dc93): Allow mDNSResponder resolver socket under network = allow (#72)
- 2026-09-04 — [c59265d](https://github.com/baiyuqing/otto/commit/c59265d1c4e596be0a2bc36096dc9de570b28ba1): Erase inline frames cleanly when suggestions and modals close (#64)
- 2026-09-04 — [4fe08ee](https://github.com/baiyuqing/otto/commit/4fe08eec197828d18d981bd6c9f647f4e99e951d): Compose TUI suggestion overlay (#63)
- 2026-09-04 — [103d55b](https://github.com/baiyuqing/otto/commit/103d55b8c145d955821768b722377074c2edf30e): Render slash suggestions as TUI overlay (#62)
- 2026-09-03 — [c6fb68d](https://github.com/baiyuqing/otto/commit/c6fb68d1d056b1cefe0306880c076e56dcff80b2): Align TUI cursor with input (#56)
- 2026-09-02 — [b0512d8](https://github.com/baiyuqing/otto/commit/b0512d839161f50dac0639e1c3b18ff6038eda24): Decode Responses API traces in the trace viewer (#49)
- 2026-09-02 — [1dd36e2](https://github.com/baiyuqing/otto/commit/1dd36e2afa0bf78ec63584fe5777a3ffb910dc84): Trace ChatGPT provider HTTP (#48)
- 2026-09-02 — [5023d9d](https://github.com/baiyuqing/otto/commit/5023d9d05ac7a94825e29bd97d3c8688d357b502): Support typical OAuth credential sizes (#47)
- 2026-09-01 — [d083e8d](https://github.com/baiyuqing/otto/commit/d083e8d7cc465c164562a40abb3adf58f1dafac9): Emit output on empty function_call_output (#42)
- 2026-08-31 — [f0a59bf](https://github.com/baiyuqing/otto/commit/f0a59bfca7453fc435a3a0fc6300b5a00aebc2bf): Always marshal content field on chat wire messages
- 2026-08-31 — [855a83c](https://github.com/baiyuqing/otto/commit/855a83c94652e3c6273410103c8d4fc8de9763c4): Stop TUI viewport from consuming typing keys
- 2026-08-30 — [8f7752d](https://github.com/baiyuqing/otto/commit/8f7752d470df1b78c2fbc585130a98e65485fc05): Finalize memory durability boundaries
- 2026-08-30 — [1027363](https://github.com/baiyuqing/otto/commit/10273634569a237dd3742ec0f1c94294f5ab07b2): Close memory core review gaps
- 2026-08-30 — [54af623](https://github.com/baiyuqing/otto/commit/54af62317a8d32a5489279a921432ff8a6f2a7b4): Harden memory lifecycle integrity
- 2026-08-30 — [c01fbe1](https://github.com/baiyuqing/otto/commit/c01fbe1c053444282add4b1d201e0cdd43c779de): Enforce bounded retrieval precedence
- 2026-08-30 — [269adab](https://github.com/baiyuqing/otto/commit/269adabe639e5f649aad32d025a3f39cefc78e56): Harden candidate transaction reconciliation
- 2026-08-30 — [aab0abb](https://github.com/baiyuqing/otto/commit/aab0abb1c87680e50662056b67b5d2829f7357c7): Report durable memory conflict revisions
- 2026-08-30 — [7932cd4](https://github.com/baiyuqing/otto/commit/7932cd471032c08fb28ea974bbadc9d0d2af9722): Harden SQLite record persistence
- 2026-08-30 — [02f563a](https://github.com/baiyuqing/otto/commit/02f563a927c12ff3693a424e1ebc29ee40e9896c): Avoid process-wide umask mutation
- 2026-08-30 — [0833888](https://github.com/baiyuqing/otto/commit/08338889807d3784f9f8392638331262acd50381): Preserve SQLite path ownership
- 2026-08-30 — [dc85909](https://github.com/baiyuqing/otto/commit/dc85909855d4735a4c5b57b9f7c6902d54e24480): Harden SQLite memory store
- 2026-08-30 — [1e30d6b](https://github.com/baiyuqing/otto/commit/1e30d6be74f295d35e8933208624da9da058cc55): Harden memory validation and guards
- 2026-08-30 — [d5764e4](https://github.com/baiyuqing/otto/commit/d5764e4f17a656099cc2de5de048b4258c41960e): Harden neutral memory contracts
- 2026-08-29 — [7742c0e](https://github.com/baiyuqing/otto/commit/7742c0ec8ce66bb9160a045893ccb205c65f2eb5): Strip stray Markdown headings from turn summaries instead of failing
- 2026-08-29 — [c91c71f](https://github.com/baiyuqing/otto/commit/c91c71ff2665500bd1db0140cffbfd5b010bd5bc): Persist empty tool output as text field in Pi records
- 2026-08-29 — [cac6a6e](https://github.com/baiyuqing/otto/commit/cac6a6e7b7d4e28eae9cb952f3d7c03d8fbefcf1): Normalize compaction fence line endings
- 2026-08-29 — [315866c](https://github.com/baiyuqing/otto/commit/315866c089034bffe09649fc8985a9397364944c): Canonicalize fence-named file paths
- 2026-08-29 — [1698055](https://github.com/baiyuqing/otto/commit/1698055f8f26a894e4469c1882c26346ff9d7107): Finalize compaction event state
- 2026-08-29 — [3241825](https://github.com/baiyuqing/otto/commit/32418256a20c004f8d39bc5a6b1ca712436e4344): Close compaction review findings
- 2026-08-29 — [56625fd](https://github.com/baiyuqing/otto/commit/56625fdbfce9cdeef94790aed67bfc1381e5e205): Correct Pi compaction probe
- 2026-08-29 — [bdf6fcb](https://github.com/baiyuqing/otto/commit/bdf6fcbebcd1118249d728fb6fd517e5f3f83d02): Release TUI workers after program failure
- 2026-08-29 — [6e51e0b](https://github.com/baiyuqing/otto/commit/6e51e0b6e9bda1d32568d4d29c8bed5ef6303400): Preserve checkpoint reconciliation order
- 2026-08-29 — [9d1c386](https://github.com/baiyuqing/otto/commit/9d1c38621806e5e3d2515ec744c95f2aca746392): Harden TUI compaction delivery
- 2026-08-29 — [e5519d2](https://github.com/baiyuqing/otto/commit/e5519d26466592e2e72fdb4ccceb68b668c52416): Harden REPL compaction cancellation
- 2026-08-29 — [58ac80d](https://github.com/baiyuqing/otto/commit/58ac80db63a3511ee7afd6db656196efd2533b19): Harden compaction lifecycle edges
- 2026-08-29 — [2f82398](https://github.com/baiyuqing/otto/commit/2f82398959f939dbe00a90f026db531ca463f9df): Preserve structured compaction summaries
- 2026-08-29 — [8c84455](https://github.com/baiyuqing/otto/commit/8c844553215f406f0591056ed2b6dcbbbe0355cc): Bound checkpoint metadata
- 2026-08-29 — [8bc70f1](https://github.com/baiyuqing/otto/commit/8bc70f1d7742f1f99e37ed4b9a8421c1bdc12691): Enforce exact summary request bounds
- 2026-08-29 — [0f55102](https://github.com/baiyuqing/otto/commit/0f551020023b91b74ec85f4c991d41e36500db27): Harden compaction cut selection
- 2026-08-29 — [7f119ee](https://github.com/baiyuqing/otto/commit/7f119ee50535e8b794f8a237e1592d2b859b41d2): Reject output token overflow false positives
- 2026-08-29 — [1496656](https://github.com/baiyuqing/otto/commit/1496656002890ceafdeaac0a34c8374c147d911f): Preserve cached prompt usage in sessions
- 2026-08-28 — [0e2cae7](https://github.com/baiyuqing/otto/commit/0e2cae765f355a93a1a5b8c49d2ccdc1a5355835): Retain git alias traversal history
- 2026-08-28 — [f4895d2](https://github.com/baiyuqing/otto/commit/f4895d2395d050d97c0872b58ef7ab32c5f7366d): Detect alternate git metadata aliases
- 2026-08-28 — [9386102](https://github.com/baiyuqing/otto/commit/938610235240b31a44b6541402b1478152af892c): Preserve lexical workspace search paths
- 2026-08-28 — [85dedb6](https://github.com/baiyuqing/otto/commit/85dedb6065dd79f6b93ac1410dcca23051906c94): Skip all git metadata roots
- 2026-08-28 — [6cb4ef4](https://github.com/baiyuqing/otto/commit/6cb4ef4d60d207307f209d8af5f04a58ecb81fb1): Bound search traversal behavior
- 2026-08-28 — [061f184](https://github.com/baiyuqing/otto/commit/061f184c7971e0902fa8b3248899c0e2b11d0271): Position macOS IME at TUI cursor
- 2026-08-28 — [c479e80](https://github.com/baiyuqing/otto/commit/c479e807b9a16d61a94074b16939c44aac0e7bfe): Preserve expanded tool details
- 2026-08-28 — [cd01eb3](https://github.com/baiyuqing/otto/commit/cd01eb31944b121a453a7da7f8e9cff5301eb409): Preserve duplicate JSON key identity
- 2026-08-28 — [4f4ee75](https://github.com/baiyuqing/otto/commit/4f4ee75501971152538c3e900ff8baed04122cbe): Preserve listing cancellation
- 2026-08-28 — [5975405](https://github.com/baiyuqing/otto/commit/5975405e4beb7475c231f1af72de1cf0dc246e69): Persist effective resumed runtime
- 2026-08-28 — [2f63e02](https://github.com/baiyuqing/otto/commit/2f63e028ab9d1c11476540ab911926d622ad91e4): Retain active-path session usage
- 2026-08-28 — [55e5ff6](https://github.com/baiyuqing/otto/commit/55e5ff64c5a19a712a8b2b7d13a642b5781f2ed9): Bound recent session inspection
- 2026-08-28 — [88a4a4f](https://github.com/baiyuqing/otto/commit/88a4a4fb09a3e51329335f11a0f907f066062127): Anchor listed session preparation
- 2026-08-27 — [ff99eb2](https://github.com/baiyuqing/otto/commit/ff99eb2154e925a7ee52e4065f9799ec08962d59): Fail closed on stale resume identity loss
- 2026-08-27 — [3fe4a44](https://github.com/baiyuqing/otto/commit/3fe4a44c03b1bbd924c19e84c48b51c57e69e4ff): Close TUI resume lifecycle gaps
- 2026-08-27 — [891d30e](https://github.com/baiyuqing/otto/commit/891d30e07f16dca69dbd4f81e9f9fe06b6726352): Harden TUI resume lifecycle
- 2026-08-27 — [c1dbb12](https://github.com/baiyuqing/otto/commit/c1dbb12b4da785d2abcc7935ebd690bdec0cc452): Harden resumed session activation
- 2026-08-27 — [c586344](https://github.com/baiyuqing/otto/commit/c586344b088c6e369c1fead270d13b57b1214a15): Defer replacement close completion
- 2026-08-27 — [0cb6812](https://github.com/baiyuqing/otto/commit/0cb6812fa299147b8d7ef11db37a200dbe6e55f0): Scope replacement close ownership
- 2026-08-27 — [289ca4f](https://github.com/baiyuqing/otto/commit/289ca4f7f213aa5d6321135b7238608304caf628): Harden session list inspection
- 2026-08-27 — [3abfcca](https://github.com/baiyuqing/otto/commit/3abfccaf2869d6964cb1f2918f5681d98559a5ac): Validate resumed tool message ordering
- 2026-08-27 — [f19a2e5](https://github.com/baiyuqing/otto/commit/f19a2e5f9f4fe9eec0cde4536f7c2b51362b4612): Harden Pi session persistence
- 2026-08-27 — [9821cde](https://github.com/baiyuqing/otto/commit/9821cdec97234c84a00b335d9db0043b4a4aff2b): Reject malformed Pi messages
- 2026-08-27 — [915ae02](https://github.com/baiyuqing/otto/commit/915ae027d8d0647322ffd7b306ab04639c4e5e84): Show tool call arguments in the TUI transcript
- 2026-08-27 — [974a7b7](https://github.com/baiyuqing/otto/commit/974a7b7244957d0e7204c1a63ece5f88bc9ff042): Preserve modified completion keys
- 2026-08-27 — [643810f](https://github.com/baiyuqing/otto/commit/643810f61162d65761ebcca07ca40a71ea8cfdcd): Bound TUI terminal strings
- 2026-08-27 — [6eca8a2](https://github.com/baiyuqing/otto/commit/6eca8a2eb813566b69a9470ff86d4ed276171ff1): Harden TUI output and tool reconciliation
- 2026-08-27 — [59015ef](https://github.com/baiyuqing/otto/commit/59015ef3f74e3222196b17de79fb781e75abcbc8): Resolve residual TUI lifecycle issues
- 2026-08-27 — [5a990ef](https://github.com/baiyuqing/otto/commit/5a990efc09a5c3b502a0dc8893064c4b86aff790): Harden TUI final review paths
- 2026-08-27 — [8c5f11f](https://github.com/baiyuqing/otto/commit/8c5f11f5f2178d59d6701c779c742a810fcd0e87): Return stable snapshot from matched footer
- 2026-08-27 — [d8881e1](https://github.com/baiyuqing/otto/commit/d8881e16905ffde92f772b08e967a785548b64d9): Cancel frontend before closing session
- 2026-08-27 — [846fae4](https://github.com/baiyuqing/otto/commit/846fae4475bb880790a7851fcd0078b09678e4ab): Harden TUI modal state races
- 2026-08-27 — [59d75c0](https://github.com/baiyuqing/otto/commit/59d75c04c93be89dc6bb64c07cd04aa91ad98ddf): Close tui turn channel after completion
- 2026-08-27 — [2e7ed99](https://github.com/baiyuqing/otto/commit/2e7ed990a92413c0f01c674f8ee675951245629d): Harden TUI turn streaming
- 2026-08-27 — [559f086](https://github.com/baiyuqing/otto/commit/559f0865469a890243a497f0bd7f4a8ef32ee0c6): Preserve empty tool entries
- 2026-08-27 — [64ab62d](https://github.com/baiyuqing/otto/commit/64ab62d61ebfb84d7edd62c76718e7266f59f49c): Preserve direct /new session errors
- 2026-08-27 — [36dc8f0](https://github.com/baiyuqing/otto/commit/36dc8f00a12343023ea9a37c21e48401d409907f): Avoid reentrant controller close deadlock
- 2026-08-27 — [3619456](https://github.com/baiyuqing/otto/commit/3619456f98bccf7e82a33276631b22fce0f6b5f1): Harden controller session replacement
- 2026-08-27 — [6f0f422](https://github.com/baiyuqing/otto/commit/6f0f42269e2afe10d21f674050f47dbe624034b1): Allow zero-block assistant session messages
- 2026-08-27 — [fb23312](https://github.com/baiyuqing/otto/commit/fb233129c004b1c23b10ef6a8c955dc6d1438c38): Address initial runtime final review
- 2026-08-27 — [15dbcb6](https://github.com/baiyuqing/otto/commit/15dbcb629eb2df2dacbab39f83f12cfa25486369): Harden CLI session cancellation lifecycle
- 2026-08-26 — [9dc1356](https://github.com/baiyuqing/otto/commit/9dc1356bd4f61b07f21822296940fdb16893e93e): Emit error event for empty agent input
- 2026-08-26 — [6144b55](https://github.com/baiyuqing/otto/commit/6144b5505a4a54843ea118b565f8c54473700957): Preserve OpenAI-compatible stream identities
- 2026-08-26 — [b8d6607](https://github.com/baiyuqing/otto/commit/b8d660744bcdd9d34e3fb8e451c7fd332b7ce46e): Return cancellation as non-error
- 2026-08-26 — [a6c9c16](https://github.com/baiyuqing/otto/commit/a6c9c16ad143731a9d1fc8f507e668a664635cdf): Keep read truncation valid utf-8
- 2026-08-26 — [75f2d85](https://github.com/baiyuqing/otto/commit/75f2d85989dc2033ccbe5a70b41cc559b0b2f5a9): Enforce explicit session wire schema
- 2026-08-26 — [1c01b03](https://github.com/baiyuqing/otto/commit/1c01b03a66abdf5c72798d48e9dcacc6c8251a3e): Require resolved API keys

</details>

<details>
<summary>Security (22 commits)</summary>

- 2026-10-04 — [cb6d186](https://github.com/baiyuqing/otto/commit/cb6d18624db8e062bf9f0132a334e1629e5bafba): Harden chat config, web UI images, prompt schedules and /approve always (#326)
- 2026-10-04 — [6ce94fa](https://github.com/baiyuqing/otto/commit/6ce94fa6f16ffec047b639f7793cb3c54f645f9b): Require approval for MCP router calls and refuse git metadata writes (#324)
- 2026-10-04 — [f90537a](https://github.com/baiyuqing/otto/commit/f90537a9d0e977dc20fb39994969834c7294c65a): Unify approval authority across runtimes and frontends (#316)
- 2026-10-03 — [2ff22d7](https://github.com/baiyuqing/otto/commit/2ff22d7a810c843055a08a51607205d032bce76f): Add approvals for sandbox read paths (#294)
- 2026-10-01 — [fe94ca5](https://github.com/baiyuqing/otto/commit/fe94ca5d6d28227dbee8a2fbd77a174a93077ca0): Make workspace git metadata read-only in the Seatbelt sandbox (#259)
- 2026-10-01 — [2ab9da9](https://github.com/baiyuqing/otto/commit/2ab9da9c19efbab2eb2beb8f22a7cbb04bc96fd3): Keep pending Bash approvals until resolved (#246)
- 2026-09-30 — [8e468f7](https://github.com/baiyuqing/otto/commit/8e468f72dc09b9d2c8dc0fca4f7b79eb5cc4c1ee): Clarify untrusted prompt content policy (#244)
- 2026-09-21 — [ede5c2f](https://github.com/baiyuqing/otto/commit/ede5c2fb64bea5c1357dcfbfaa058283b6f91031): Close two authorization holes: sandbox read paths and Feishu inbound (#150)
- 2026-09-05 — [148b603](https://github.com/baiyuqing/otto/commit/148b60359caf34925b6c052390403791f48760da): Harden workspace access, startup execution, and HTTP traces (#68)
- 2026-08-30 — [7e41594](https://github.com/baiyuqing/otto/commit/7e41594b03a9d860016b5b880a7e45e63c8fdd63): Broaden .gitignore to block secret and credential files
- 2026-08-29 — [344c9e4](https://github.com/baiyuqing/otto/commit/344c9e4d06c34af063d57ee50cd2858021687260): Bump golang.org/x/net to v0.56.0
- 2026-08-29 — [d9c4ca4](https://github.com/baiyuqing/otto/commit/d9c4ca4b8d9455a6f5520487e314de7c2fad3c76): Bound --approve prompt files to 1 MiB
- 2026-08-29 — [ec7161b](https://github.com/baiyuqing/otto/commit/ec7161b1c52b401a5b44bdf6a6bfdf64d160f960): Bound read and edit to a maximum file size
- 2026-08-29 — [c9a0535](https://github.com/baiyuqing/otto/commit/c9a053515249822aa587901b7e9d07854b961700): Harden default provider HTTP client
- 2026-08-29 — [ae315d0](https://github.com/baiyuqing/otto/commit/ae315d0a8d8e75227dbeae167d465d37dc9b319a): Upgrade dependencies to patched releases
- 2026-08-28 — [ed606bd](https://github.com/baiyuqing/otto/commit/ed606bdde02dc9ce9b758f3fc8c4c74dd2bf1ed4): Redact secret-bearing JSON keys
- 2026-08-28 — [9cf4b2c](https://github.com/baiyuqing/otto/commit/9cf4b2cdeae9e53d9f603cf6454fe2f246a3c34b): Prevent synthesized secret output
- 2026-08-28 — [c9de811](https://github.com/baiyuqing/otto/commit/c9de8116140ab809861dd02a938996e44bade2f0): Harden agent secret replacement
- 2026-08-28 — [1ad8fd6](https://github.com/baiyuqing/otto/commit/1ad8fd6dc1e184ff9feac53e8dff483dd4415bcc): Redact provider output at agent boundary
- 2026-08-28 — [15c5fed](https://github.com/baiyuqing/otto/commit/15c5fed4ebfa4892ce0a61f933b00fe49743cde7): Fail closed on forged resume state
- 2026-08-27 — [a5ce7bf](https://github.com/baiyuqing/otto/commit/a5ce7bf61bb22d0e7a20ee402fd9525307d0ef2e): Eliminate session no-follow races
- 2026-08-26 — [3207fc9](https://github.com/baiyuqing/otto/commit/3207fc9a9873de80ce2ee128741322454c46253d): Enforce workspace tool boundaries

</details>

<details>
<summary>Performance (6 commits)</summary>

- 2026-10-04 — [c102f8e](https://github.com/baiyuqing/otto/commit/c102f8e3f6ec43ef1bfe335d028f534085125b4e): Fix idle ACP CPU usage (#318)
- 2026-10-01 — [d7314ae](https://github.com/baiyuqing/otto/commit/d7314ae8c6e6d16973a737cc139c3f86ea41f1b5): Keep skill lists compact (#250)
- 2026-09-22 — [f15f2e6](https://github.com/baiyuqing/otto/commit/f15f2e682a5faf06689a27e1466a515a90ca72e8): Fix slow interactive exit (#171)
- 2026-09-22 — [7f3da05](https://github.com/baiyuqing/otto/commit/7f3da059b6d35ac8ac871e204f8bcc8af2667283): Start interactive sessions before MCP connects (#169)
- 2026-09-22 — [36cd402](https://github.com/baiyuqing/otto/commit/36cd402c78557e1f3d212e91a00335be36f578bb): Cli: connect MCP servers concurrently (#168)
- 2026-09-02 — [00f1b9a](https://github.com/baiyuqing/otto/commit/00f1b9a798795d700d7ff01f836b5832c27202dc): Cut per-request prompt cost and close two system-prompt exposures (#52)

</details>

<details>
<summary>Build and CI (42 commits)</summary>

- 2026-10-05 — [3be07a8](https://github.com/baiyuqing/otto/commit/3be07a83c87f9a063393a15153036d3034997a30): Cut CI time: stop recompiling, cache only dependencies, speed up slow tests (#330)
- 2026-10-04 — [39632b7](https://github.com/baiyuqing/otto/commit/39632b768c95305fef4fee85da9a8b9a7c8c90dd): Embed the git commit in the version and add a manual release procedure (#322)
- 2026-10-04 — [4260a3d](https://github.com/baiyuqing/otto/commit/4260a3d0f4c8dbbe3b5613d2e2804c2788b38065): Remove duplicate builds from make check (#321)
- 2026-10-04 — [ba1ad3c](https://github.com/baiyuqing/otto/commit/ba1ad3c34856b14b046ddb1ba6216d583133fb0a): Remove Cargo build outputs in make clean (#311)
- 2026-10-04 — [ada7d7f](https://github.com/baiyuqing/otto/commit/ada7d7fad9e5afc4e65706ce58244a6f2e0faa63): Prevent fake ACP agent deadlock during close tests (#309)
- 2026-10-03 — [611b5e8](https://github.com/baiyuqing/otto/commit/611b5e83d6e0c41aea5eda95740d5503435f3237): Wait for blocked fake prompts before the fake agent exits (#299)
- 2026-10-02 — [e378a13](https://github.com/baiyuqing/otto/commit/e378a13029b96ded55e56a3f448f2444f87e41ec): Build and install otto-connect with make install (#282)
- 2026-09-30 — [2abf2e1](https://github.com/baiyuqing/otto/commit/2abf2e1323ce6a2a809ff116a311a9586b746df1): Stabilize concurrent lease acquisition test (#247)
- 2026-09-30 — [a7070df](https://github.com/baiyuqing/otto/commit/a7070dfe618107ba8da5ab22d37339b76965df61): Fix stale TUI footer assertions (#240)
- 2026-09-29 — [73a34ea](https://github.com/baiyuqing/otto/commit/73a34eadbfe98fd7c67e60f3ac9b253738500c52): Fix Linux clippy dead_code on the default-grace process runner (#237)
- 2026-09-28 — [97b5ca5](https://github.com/baiyuqing/otto/commit/97b5ca5e6104328df825504d020a7fd49bdd6be9): Fix stale Wasm tool transcript fixture (#233)
- 2026-09-28 — [99f2adc](https://github.com/baiyuqing/otto/commit/99f2adc9d720146f121530e77c851429a6eb668a): Wait for the current busy composer title in the TUI PTY test (#231)
- 2026-09-28 — [9bcff68](https://github.com/baiyuqing/otto/commit/9bcff68a1a5bc23ef0bf3263def4b1bee8040210): Update agent_send schema fixture (#229)
- 2026-09-27 — [565d16b](https://github.com/baiyuqing/otto/commit/565d16b65090b0137b6398dd74686e15a95fa9a8): Add make desktop-app for a local ad-hoc signed build (#202)
- 2026-09-26 — [759f1e0](https://github.com/baiyuqing/otto/commit/759f1e00862054770b694df38af3879079533a1c): Fix CI after agent_send tool addition (#194)
- 2026-09-22 — [92156b6](https://github.com/baiyuqing/otto/commit/92156b6308ae09ab05092015c8c51e1d8a44aed1): Measure what sub-agent execution of skills is worth (#181)
- 2026-09-21 — [6f4b850](https://github.com/baiyuqing/otto/commit/6f4b850d1032749f966f09beff5d8a085048e8eb): Fix workflow scheduling tests broken by write coordination (#163)
- 2026-09-21 — [4d21faf](https://github.com/baiyuqing/otto/commit/4d21faf55f016f665a3bc520b3739522dedd7c33): Install Otto to local bin (#149)
- 2026-09-19 — [37437bf](https://github.com/baiyuqing/otto/commit/37437bfa080a16da4b0baa261d3ed3ed1ffa5574): Build current UI and support image-only turns (#126)
- 2026-09-17 — [272dc25](https://github.com/baiyuqing/otto/commit/272dc25a5b033c54f483c62ba7e887899284d993): Make cargo detect ui/dist changes for the embedded UI (#122)
- 2026-09-17 — [ddcd938](https://github.com/baiyuqing/otto/commit/ddcd938da9e080b7a5867df1c010f9a58aea22a7): Add @types/node so tsc resolves node: imports (#121)
- 2026-09-14 — [ac2bbba](https://github.com/baiyuqing/otto/commit/ac2bbba9da19f0faf76815078949e32f4616a22c): Handle PTY startup output (#97)
- 2026-09-05 — [231854b](https://github.com/baiyuqing/otto/commit/231854b1488735de15a580027cdc365ce0cc5664): Accept cursor-line background SGR in PTY placeholder tests (#74)
- 2026-09-02 — [44977c9](https://github.com/baiyuqing/otto/commit/44977c9b235a05b98c4e166793420abdf5886de0): Prevent login flows from opening browsers (#46)
- 2026-08-30 — [1417d97](https://github.com/baiyuqing/otto/commit/1417d978148df6dc79ad6bf624dac409f9701d2e): Gate memory concurrency at SQL seams
- 2026-08-30 — [867c229](https://github.com/baiyuqing/otto/commit/867c229c9a3830bedf2184286e9975ac14711989): Strengthen SQLite expiry corruption coverage
- 2026-08-30 — [4eaf668](https://github.com/baiyuqing/otto/commit/4eaf668ba688ef08c34bd552843a3033afd28f72): Pin SQLite mkdir umask isolation
- 2026-08-30 — [946c53a](https://github.com/baiyuqing/otto/commit/946c53aab5f71bc7d4045168edd36ab2346e45f5): Strengthen SQLite umask regression
- 2026-08-29 — [45cc9df](https://github.com/baiyuqing/otto/commit/45cc9dfd7fa4dc3500568b7656f846825356bb4e): Cover footer token boundaries
- 2026-08-29 — [ef5ed52](https://github.com/baiyuqing/otto/commit/ef5ed52a1903625bd80acbbb403980c63d3d35f6): Assert boxed composer cursor column
- 2026-08-29 — [c82799f](https://github.com/baiyuqing/otto/commit/c82799f53724e9b37d80ff79a5c637882d0b4fae): Recognize boxed composer SGR
- 2026-08-29 — [e4762ef](https://github.com/baiyuqing/otto/commit/e4762efc1c030459e54deeb5c0337f724ccaea89): Cover session usage saturation
- 2026-08-28 — [f57e0b2](https://github.com/baiyuqing/otto/commit/f57e0b2f12685a419ef43869811f0c94edc30cc6): Add Makefile with build and CI targets
- 2026-08-28 — [8377b21](https://github.com/baiyuqing/otto/commit/8377b214ec72ed42b60a7797a0255a89b0b3f30d): Accept layered redaction markers
- 2026-08-28 — [12dfdf2](https://github.com/baiyuqing/otto/commit/12dfdf28c62bfb58d78cb04efb1f4a1dcfa0205a): Fail closed on unsupported terminal input
- 2026-08-28 — [9c3af3f](https://github.com/baiyuqing/otto/commit/9c3af3f9bf33b22f1d1e5675f143ee26397fe031): Prove resume redraw terminal state
- 2026-08-28 — [1b29d0a](https://github.com/baiyuqing/otto/commit/1b29d0af6f41f5dd9972a84420f62a371d6248f8): Harden resume integration evidence
- 2026-08-28 — [74be7e5](https://github.com/baiyuqing/otto/commit/74be7e5002a13344723c369341641718c7540751): Harden resume picker rendering tests
- 2026-08-27 — [7b7db65](https://github.com/baiyuqing/otto/commit/7b7db650da4c3cd7d1633e816fa170e0fe923ac9): Cover completion state transitions
- 2026-08-27 — [15e2c62](https://github.com/baiyuqing/otto/commit/15e2c6205f112005531e607674fc0ee80f47b440): Stabilize PTY resize assertion
- 2026-08-27 — [eea0410](https://github.com/baiyuqing/otto/commit/eea041078ea586a712967cd4a7d9315e9b408caf): Harden PTY lifecycle smoke coverage
- 2026-08-27 — [f9166e7](https://github.com/baiyuqing/otto/commit/f9166e7c7eaa15e1e8cd3afb5d70948419796d40): Cover TUI terminal lifecycle

</details>

<details>
<summary>Documentation (42 commits)</summary>

- 2026-10-05 — [89064ec](https://github.com/baiyuqing/otto/commit/89064eca66b409007fac66c8c53f7fc138edc801): Add categorized changelog and maintenance rules
- 2026-10-04 — [75678aa](https://github.com/baiyuqing/otto/commit/75678aa8249f4108b5b4f2364f2de7f09cb518d0): Clarify Otto identity and enable chat reflection (#303)
- 2026-10-03 — [7e7bde1](https://github.com/baiyuqing/otto/commit/7e7bde1762028935b3acec3f2441755f75257f5b): Clarify general agent identity and chat channel context (#296)
- 2026-10-03 — [7cf4696](https://github.com/baiyuqing/otto/commit/7cf469689b07ea67cf64ad75410b1afc4f8c3d7a): Cache-bust the README logo (#291)
- 2026-10-03 — [bb5a3fb](https://github.com/baiyuqing/otto/commit/bb5a3fbc3fb1b8ddd73a91e26d34eb4a41b9124e): Add code organization map (#288)
- 2026-09-28 — [7f75e05](https://github.com/baiyuqing/otto/commit/7f75e05c11c8d51a7fad010a71a3312f6cb1254f): Remove the unrun NFSv4 failover acceptance procedure (#225)
- 2026-09-27 — [7e424e7](https://github.com/baiyuqing/otto/commit/7e424e70e83b53c20b8c8e78839cce8d1021db59): Mark sidebar follow-ups spec approved
- 2026-09-26 — [fed5503](https://github.com/baiyuqing/otto/commit/fed5503f947d96226ad5c4cc79598a733100c302): Add spec: remove a workspace, stop status reconnects on 401, refresh Changes after a turn
- 2026-09-25 — [04cb2a5](https://github.com/baiyuqing/otto/commit/04cb2a553d652323039c7c98429ad0433e67b421): Add design for a cross-session sub-agent view
- 2026-09-22 — [1c5c46e](https://github.com/baiyuqing/otto/commit/1c5c46ed6dbbb1d3d473e0a898e8b3f7a7d91eff): Realign the manual and dev guide with the code, and skip CI for docs-only changes (#177)
- 2026-09-22 — [2a79b63](https://github.com/baiyuqing/otto/commit/2a79b63a6dcf9e50961ef384ee8b0660ba114f23): Realign the manual, dev guide, and --help with the code (#164)
- 2026-09-17 — [4255c9f](https://github.com/baiyuqing/otto/commit/4255c9fa45eb9f0fa6f8a65846709043d8fa90dc): Keep gitignored worktrees in the Cursor agent workspace (#117)
- 2026-09-14 — [719cdb2](https://github.com/baiyuqing/otto/commit/719cdb21e352c5623254394e889cdb2fcc2e523a): Document the responses the server already returns (#105)
- 2026-09-14 — [395bedd](https://github.com/baiyuqing/otto/commit/395beddebc77768e881092e5499c54b65d88e193): Name Rust symbols in the development guide contracts (#93)
- 2026-09-05 — [ecafc13](https://github.com/baiyuqing/otto/commit/ecafc1355f1bce546e0878cd781ed3bd09706f4a): Make development guidance and validation agent-friendly (#71)
- 2026-09-05 — [e87bfab](https://github.com/baiyuqing/otto/commit/e87bfab7abd3d6fae1b42880ca97466401d5bb74): Improve project discoverability and quick start (#67)
- 2026-09-04 — [51db08b](https://github.com/baiyuqing/otto/commit/51db08bb571edb7156a7dd3e0fb6674fc0b2bf98): Remove roadmap framing and document the chatgpt provider (#66)
- 2026-09-04 — [cde0534](https://github.com/baiyuqing/otto/commit/cde05347c950a181a258ec059fccd9edf914b5a3): Refresh README for GA (#65)
- 2026-09-01 — [2def1ba](https://github.com/baiyuqing/otto/commit/2def1babb88e86073d60e4ad3f90ce0be34b9f4c): Mention AGENTS instructions in system prompt
- 2026-08-31 — [11e1e49](https://github.com/baiyuqing/otto/commit/11e1e490aa53039b78f4cdd7d5ab01f2ff3d1faa): Update agents.md
- 2026-08-31 — [3fe95d7](https://github.com/baiyuqing/otto/commit/3fe95d7102d688339259bb6c63a8d266afffa72b): Document memory core status and add user manual
- 2026-08-30 — [73c4e0b](https://github.com/baiyuqing/otto/commit/73c4e0b2654e8830d6b23f4b08428b3c1712b118): Plan extensible memory core
- 2026-08-30 — [c776308](https://github.com/baiyuqing/otto/commit/c776308609148a2bb92fcca18a21f8350c847ef0): Clarify memory core contracts
- 2026-08-30 — [59513d3](https://github.com/baiyuqing/otto/commit/59513d36ec7628a756e1b7ff96593d24d677bc9a): Design extensible memory subsystem
- 2026-08-30 — [f8ec175](https://github.com/baiyuqing/otto/commit/f8ec17555414fb4a554651c07cd1d0fa1fd56287): List grep/find/ls in tool boundary and confinement docs
- 2026-08-30 — [20f61fa](https://github.com/baiyuqing/otto/commit/20f61fa86d19d5cf32a68dc84c4484b839ef3cc0): Replace logo with cleaner version without tagline
- 2026-08-30 — [d081133](https://github.com/baiyuqing/otto/commit/d0811332b32404f0211fa7bb3c8586aab16d4718): Add otto logo to README
- 2026-08-29 — [8abfadc](https://github.com/baiyuqing/otto/commit/8abfadcf6f9f813a39fbf3d4e9cfd459ce383102): Document read/edit size cap and --approve file cap
- 2026-08-29 — [1e7308a](https://github.com/baiyuqing/otto/commit/1e7308abc476713ad13b605689c1aebde9b17fd0): Document context compaction
- 2026-08-28 — [f04ff2b](https://github.com/baiyuqing/otto/commit/f04ff2b79456aae2312700cd9e0d1a53dee9382b): Plan context compaction
- 2026-08-28 — [2f69135](https://github.com/baiyuqing/otto/commit/2f69135ab33d431e4583608b63f186470c7ae46c): Design context compaction
- 2026-08-28 — [bf0dee6](https://github.com/baiyuqing/otto/commit/bf0dee6964266a09d76427863e5d34a35e40e205): Add --thinking and --approve to README feature list and quick start
- 2026-08-28 — [39ebee4](https://github.com/baiyuqing/otto/commit/39ebee445cf21b999d900bbd89432defe7ffd1ed): Document Pi sessions and resume
- 2026-08-27 — [9e03b4f](https://github.com/baiyuqing/otto/commit/9e03b4fe486629ab11c507aafe88e929038da8fa): Plan Pi v3 sessions and resume
- 2026-08-27 — [4e3ef1f](https://github.com/baiyuqing/otto/commit/4e3ef1fb23288be25156edf6459f15686ec8bff4): Design Pi v3 sessions and resume
- 2026-08-27 — [c31138e](https://github.com/baiyuqing/otto/commit/c31138ee368be0c112722c7cd69f86e1bd2a4a02): Document adaptive Charmbracelet TUI
- 2026-08-27 — [22356d2](https://github.com/baiyuqing/otto/commit/22356d26023ed95cf268a23d7bf26599ee65e5bf): Plan Charmbracelet TUI implementation
- 2026-08-27 — [004a805](https://github.com/baiyuqing/otto/commit/004a805183d1fa98e20154f916f33c69366b42a0): Design Charmbracelet TUI
- 2026-08-27 — [6dd9e13](https://github.com/baiyuqing/otto/commit/6dd9e13c66d3b89d43d491856141b13801f0266a): Clarify config discovery
- 2026-08-27 — [77c0d07](https://github.com/baiyuqing/otto/commit/77c0d073f8c52280e5f5a4e63880942c575f4d98): Document the initial Otto runtime
- 2026-08-26 — [33f7396](https://github.com/baiyuqing/otto/commit/33f7396f987e0b28f339db4870782b30281b5fa2): Add the initial Otto runtime implementation plan
- 2026-08-26 — [c582761](https://github.com/baiyuqing/otto/commit/c58276187e2f0a39310211dcbcb2addebfdbf126): Add Otto coding agent design

</details>

<details>
<summary>Internal maintenance and merges (45 commits)</summary>

- 2026-09-20 — [c8ddca8](https://github.com/baiyuqing/otto/commit/c8ddca8054ff3c0ff13f46e06d8f33e196ca7250): Remove dead Go provenance from comments and test names (#145)
- 2026-09-15 — [a07d259](https://github.com/baiyuqing/otto/commit/a07d259a857d99e9f36fdcd72fbfdcefefcc9ab1): Enforce the empty-list argument convention in one place (#108)
- 2026-09-05 — [44d49d5](https://github.com/baiyuqing/otto/commit/44d49d59c629d9249341493a84324c4b672e4c18): Clarify core data and application contracts (#70)
- 2026-09-02 — [cc258cc](https://github.com/baiyuqing/otto/commit/cc258cc3eb01f918035ee848c9024b71c66c5898): Remove useless files
- 2026-09-01 — [8129e03](https://github.com/baiyuqing/otto/commit/8129e03a40a6b6d1fd41bbac987934a557bce96c): Remove superpowers*
- 2026-08-31 — [bd8bb46](https://github.com/baiyuqing/otto/commit/bd8bb4698649bb3ef54ac6ade50b8b7d1ff51aeb): Merge pull request #33 from baiyuqing/fix/openaicompat-missing-content-field
- 2026-08-31 — [8d160e8](https://github.com/baiyuqing/otto/commit/8d160e8e6b4da7bed8884448fd563dee96bf8a78): Merge pull request #32 from baiyuqing/fix/tui-space-scroll-typing
- 2026-08-31 — [71e95b1](https://github.com/baiyuqing/otto/commit/71e95b15462a5d4da4e9a627fb731ddf37644c1b): Merge pull request #31 from baiyuqing/docs/user-manual-and-memory-docs
- 2026-08-31 — [4d634b8](https://github.com/baiyuqing/otto/commit/4d634b83ad90a7153052258daa720ec5c14168d6): Merge pull request #30 from baiyuqing/feature/extensible-memory-core
- 2026-08-30 — [cde04d0](https://github.com/baiyuqing/otto/commit/cde04d080d362e85754fb5dba4260a53f4f9b9b9): Merge pull request #29 from baiyuqing/feat/compaction-plan-event
- 2026-08-30 — [126c4b5](https://github.com/baiyuqing/otto/commit/126c4b5f736f0653a108b002b92c98696004324a): Merge pull request #28 from baiyuqing/feat/tui-logo
- 2026-08-30 — [8a44326](https://github.com/baiyuqing/otto/commit/8a44326282fe44bf2d7768f6cb1e238c3257355b): Merge pull request #27 from baiyuqing/feat/ascii-logo-startup
- 2026-08-30 — [2f1fc71](https://github.com/baiyuqing/otto/commit/2f1fc71d1bf2daeba946e6b9c5bf5e0ad6ce5854): Merge pull request #26 from baiyuqing/docs/add-logo
- 2026-08-30 — [3366a86](https://github.com/baiyuqing/otto/commit/3366a865f4b7998df2816e22c668c9826d018129): Merge pull request #25 from baiyuqing/feat/otto-mark
- 2026-08-30 — [62f887c](https://github.com/baiyuqing/otto/commit/62f887ce3c992dac1afef843e31569c8a4079e1d): Merge pull request #24 from baiyuqing/docs/add-logo
- 2026-08-29 — [e19e86a](https://github.com/baiyuqing/otto/commit/e19e86af34db00ae7c941c03e3e6824abcb22033): Merge pull request #23 from baiyuqing/feature/human-readable-status
- 2026-08-29 — [2f213b3](https://github.com/baiyuqing/otto/commit/2f213b3ce846321f7eab556f3d75644e3d3254a6): Merge pull request #22 from baiyuqing/fix/pty-boxed-input-sgr
- 2026-08-29 — [f4ab65f](https://github.com/baiyuqing/otto/commit/f4ab65f71309258bcaee6a9256dd4629b6a52f5f): Merge pull request #21 from baiyuqing/baiyuqing/tui-input-box
- 2026-08-29 — [0d667b9](https://github.com/baiyuqing/otto/commit/0d667b98a0c8432cd68a997e8c95d372f86eba07): Merge pull request #20 from baiyuqing/baiyuqing/tui-transcript-clarity
- 2026-08-29 — [ddf19dd](https://github.com/baiyuqing/otto/commit/ddf19dde1d9b4921afe5a262c2ee8c530b2c0b0a): Merge pull request #19 from baiyuqing/fix/security-hardening
- 2026-08-29 — [1c44423](https://github.com/baiyuqing/otto/commit/1c44423cf355ee7fb9489ae525aa4ffa5f77b2d3): Merge pull request #18 from baiyuqing/fix/compaction-summary-headings
- 2026-08-29 — [f5fb30a](https://github.com/baiyuqing/otto/commit/f5fb30a77d5c46109738551467ecd4835563fa8d): Merge pull request #17 from baiyuqing/feature/tui-high-value
- 2026-08-29 — [77ba2b1](https://github.com/baiyuqing/otto/commit/77ba2b1ff8d4f22e91a124cd7a46c0907792021a): Merge pull request #16 from baiyuqing/fix/empty-tool-output
- 2026-08-29 — [847ba69](https://github.com/baiyuqing/otto/commit/847ba6968178ae4a7acbfff929f2d1aad8826bbb): Merge pull request #15 from baiyuqing/feature/context-compaction
- 2026-08-29 — [9b33b12](https://github.com/baiyuqing/otto/commit/9b33b12456c2710d35579832a44e167a618baddf): Organize compaction summary helpers
- 2026-08-28 — [2205f46](https://github.com/baiyuqing/otto/commit/2205f46fd9a21a8b235f70edd3180f86299862b3): Merge pull request #14 from baiyuqing/feature/cached-token-usage
- 2026-08-28 — [c900dd8](https://github.com/baiyuqing/otto/commit/c900dd8bab4fd71d2b2a987f70a0a9cac535a29d): Merge pull request #12 from baiyuqing/feature/search-tools
- 2026-08-28 — [02b15c0](https://github.com/baiyuqing/otto/commit/02b15c03256cfbd0cf5120455b098ffb5b7bb7a2): Merge pull request #13 from baiyuqing/docs/readme-thinking-approve-examples
- 2026-08-28 — [3218cf7](https://github.com/baiyuqing/otto/commit/3218cf7b0454e4d0be174abc79f32fbe9d522bd0): Merge pull request #11 from baiyuqing/feature/thinking-approve-flags
- 2026-08-28 — [efdb431](https://github.com/baiyuqing/otto/commit/efdb431634e393f5cff7e7e4a340e426f36b44a9): Merge pull request #10 from baiyuqing/feature/edit-tool-diff-results
- 2026-08-28 — [c6a423b](https://github.com/baiyuqing/otto/commit/c6a423b6748f0c2f9c7d1a57489f4533d4632a94): Merge pull request #9 from baiyuqing/fix/ime-cursor-position
- 2026-08-28 — [40eaa4d](https://github.com/baiyuqing/otto/commit/40eaa4d731e9a3930e86f6c3377309b5f5911ff8): Merge pull request #8 from baiyuqing/feature/lazy-sessions-and-makefile
- 2026-08-28 — [3f74113](https://github.com/baiyuqing/otto/commit/3f741134910e96ad9c3ea447e111ad358e415188): Merge pull request #7 from baiyuqing/feature/remove-max-turns
- 2026-08-28 — [6ac3199](https://github.com/baiyuqing/otto/commit/6ac31991235322ff0ee35cda5092180f8f1839e6): Merge pull request #6 from baiyuqing/feature/concise-tool-output
- 2026-08-28 — [43bc7c4](https://github.com/baiyuqing/otto/commit/43bc7c467d1fb320cf7a13daa185bf87b7919831): Merge pull request #5 from baiyuqing/feature/pi-v3-sessions-resume
- 2026-08-27 — [63bf66b](https://github.com/baiyuqing/otto/commit/63bf66b3d5f7ee6f5582adfec870b138cbe36941): Share session runtime construction
- 2026-08-27 — [5777c0b](https://github.com/baiyuqing/otto/commit/5777c0b3be4da320e90bdf226c3bd4cbd7ae159e): Merge pull request #4 from baiyuqing/feature/tool-details-and-max-turns
- 2026-08-27 — [692b718](https://github.com/baiyuqing/otto/commit/692b7183804d931e5b6dbd309d40d16ab3342fc2): Merge pull request #3 from baiyuqing/feature/command-completion
- 2026-08-27 — [1c1aa6e](https://github.com/baiyuqing/otto/commit/1c1aa6e72efdd924d338570c03eed9ba553700a0): Merge pull request #2 from baiyuqing/feature/charmbracelet-tui
- 2026-08-27 — [e98c24d](https://github.com/baiyuqing/otto/commit/e98c24d0689ddace6cbe1940f402b842e43d3247): Remove TUI review artifacts
- 2026-08-27 — [7b6e36b](https://github.com/baiyuqing/otto/commit/7b6e36be9d003dd72b71a7f21632caa00da93d30): Share app lifecycle across frontends
- 2026-08-27 — [398b5cd](https://github.com/baiyuqing/otto/commit/398b5cdacdb8717c85e5384b8f147f36beb6dbcc): Add shared application controller
- 2026-08-27 — [c1b0c89](https://github.com/baiyuqing/otto/commit/c1b0c89932def040ad1c74a2eb1c0ed1a81771b4): Remove internal review artifacts
- 2026-08-26 — [4273e0a](https://github.com/baiyuqing/otto/commit/4273e0ab7629958674f3233c1d68e24bad46484d): Add neutral agent contracts
- 2026-08-26 — [0508987](https://github.com/baiyuqing/otto/commit/0508987d9e1f1ea82444fcaf453d771e130f1020): Ignore agent workspaces

</details>
