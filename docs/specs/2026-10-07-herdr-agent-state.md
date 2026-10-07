# Herdr agent-state reporting

Status: proposed 2026-10-07, amended 2026-10-07 after a live session: the pane
is claimed as soon as the frontend starts (State mapping) and Otto is addressed
by pane id (Decisions 5). Maintainer decisions on 2026-10-07: report only,
implemented inside `crates/otto`, because Herdr cannot be changed at this
stage; **no configuration key** (the Herdr environment is the only switch);
the reported label is always `otto`.

## Problem

Herdr is a terminal workspace manager for coding agents. A pane it manages is
"nobody" until Herdr learns which agent occupies it: no agent row, no
`idle`/`working`/`blocked` state, no `herdr agent wait`, no notification when
the agent needs a human. Facts from Herdr 0.9.3 and this checkout at `e065d23`:

- Herdr recognises the agents it ships an integration for (`pi`, `claude`,
  `codex`, `gemini`, `cursor`, …), either through a screen detector
  (detection manifest, refreshed from `herdr.dev`) or through lifecycle
  reports from the agent's own hook or plugin. The agent side of every bundled
  integration is the same socket protocol; `~/.claude/hooks/herdr-agent-state.sh`
  and `~/.pi/agent/extensions/herdr-agent-state.ts` are two spellings of it.
- Otto is not one of them, and cannot be added from here:
  `herdr agent start --kind otto` rejects the kind (the `--kind` list is
  fixed), and `herdr integration install otto` prints
  `unknown integration target: otto`. Both need a Herdr-bundled detector or
  integration.
- The remaining door is the documented custom-integration channel: an agent
  that Herdr does not know may report its own lifecycle and Herdr treats that
  report as the authority ("They don't need to be built into Herdr, and Herdr
  doesn't need to recognize the agent's executable" —
  <https://herdr.dev/docs/integrations/#integrate-your-own-agent>).
- Inside a pane Herdr injects `HERDR_ENV=1`, `HERDR_PANE_ID`,
  `HERDR_SOCKET_PATH` (and the workspace/tab/pane ids). Everything else about
  the protocol is public: `herdr api schema` describes `pane.report_agent`,
  `pane.report_agent_session`, `pane.release_agent` and `pane.report_metadata`,
  and `herdr pane report-agent --help` describes the same operations as CLI
  verbs.

Otto already has everything the report needs, on the native side only:
`Event::{AgentStarted, AgentFinished, ApprovalChanged}` reach every frontend
sink (`crates/otto-core/src/agent/events.rs`), `app::Info` carries
`session_id` and `session_path` (`crates/otto/src/app/mod.rs`), and
`otto --resume PATH` reopens a session file
(`crates/otto/src/cli/flags.rs`).

## Goals

1. Inside a Herdr pane, Otto identifies itself to Herdr and keeps its
   lifecycle state current: `working` while a turn runs, `blocked` while it
   waits for a human decision, `idle` when it is ready for input.
2. Report the session reference Herdr needs to show and restore the session:
   `agent_session_id`, `agent_session_path`, and `resume_argv`
   (`otto --resume <path>`).
3. Make `herdr agent wait <pane> --until blocked`, notifications and pane jump
   work for Otto, so a user notices an approval while working elsewhere.
4. No effect outside Herdr, and no new model-callable capability.
5. On by default with nothing to configure: inside a Herdr pane the
   environment is the only switch.

## Non-goals

- **A first-class Herdr kind.** `herdr agent start --kind otto`,
  `herdr integration install otto` and Herdr's own detector stay out of reach
  without a change on the Herdr side. Users start Otto with a normal pane
  command instead (`herdr pane run <pane> otto`, or typing `otto` in a split).
- **Changing Herdr.** No dependency on a Herdr release, no bundled binary, no
  Herdr-side configuration file.
- **A Herdr agent name.** Herdr keeps the name alias (`herdr agent wait otto`)
  for the names it assigns itself or the user sets with `herdr agent rename`; a
  report carries the label `otto` only, so Otto is addressed by its pane id.
  Verified in a live session on 2026-10-07: the panel and `herdr agent list`
  show `otto`, while `herdr agent get otto` reports `agent_not_found` until the
  name is set.
- **Detecting Herdr from outside a pane.** Only the process Herdr started
  inside the pane can report; Otto does not scan for other panes.
- **Reading Otto's screen for state.** State comes from the event stream, not
  from scraping the TUI. Herdr's `agent read` limitations on the alternate
  screen (see Limitations) are not addressed.
- **First release: `otto serve`, `otto acp`, `--attach` clients and headless
  `Once` runs.** They are not the pane's foreground agent. `--attach` can be
  added later from the same sink decoration.

## Design

### Contract with Herdr

Reporting is inert unless all three hold: `HERDR_ENV == "1"`,
`HERDR_PANE_ID` non-empty, `HERDR_SOCKET_PATH` non-empty. Otherwise the module
is never constructed and no thread, task or socket exists.

One newline-delimited JSON request per report over `HERDR_SOCKET_PATH`
(`tokio::net::UnixStream`, already available: `crates/otto/Cargo.toml` enables
tokio's `net` feature), matching the bundled integrations:

```json
{"id":"custom:otto:<n>","method":"pane.report_agent","params":{
  "pane_id":"w1:p1","source":"custom:otto","agent":"otto",
  "state":"working","message":null,"seq":7,
  "agent_session_id":"<id>","agent_session_path":"<path>",
  "resume_argv":["otto","--resume","<path>"]}}
```

- `pane.report_agent` — lifecycle state, one of `idle`, `working`, `blocked`,
  `unknown` in the request schema. Otto sends only the first three; `done` is
  derived by Herdr from an unseen `idle` and must not be sent.
- `pane.report_agent_session` — session reference, sent when a session is
  opened and after a resume or fork.
- `pane.release_agent` — on exit, so Herdr keeps no stale row.
- `source` is the stable integration id `custom:otto`, as the integrations
  guide requires; `agent` is the label `otto`. `seq` is a process-local
  strictly increasing counter, so a late report can never re-attach an agent
  Herdr already released.
- Every failure is silent and bounded: the socket write has a 0.5 s timeout,
  a missing socket or a refused connection is dropped, and nothing is logged
  at warning level. Otto's behaviour must not depend on Herdr being reachable.

### State mapping

| Otto event | Reported state |
| --- | --- |
| Otto's frontend is ready (the session is open) | `idle`, immediately, then the session reference |
| A logical turn is admitted (`Controller` run entry) | `working` |
| `Event::ApprovalChanged` with `request.state == State::Pending` | `blocked`, `message` = the request title, e.g. `Run rm -rf build` |
| That request leaves `Pending` (`Approved`, `Rejected`, `Withdrawn`, `Expired`) while the turn still runs | `working` |
| The logical turn ends (success, failure or cancellation) | `idle` |
| Otto exits | `release_agent` |

- The pane is claimed when the frontend starts, not at the first turn: a Herdr
  pane is "nobody" until the agent inside it reports, so Otto would be missing
  from the agent list and unreachable through `herdr agent ...` until the user
  sent a prompt. That first `idle` is not debounced.
- The state always goes before the session reference: Herdr answers
  `pane.report_agent_session` with `resume_not_accepted` ("resume_argv requires
  the reporter to hold the pane; report its state with pane.report_agent
  first") when no state report preceded it. A session reference that is
  reported for a resume or fork is therefore only sent from a reporter that
  already holds the pane.
- The state is keyed on the logical turn, not on a single agent run: one turn
  may contain follow-up steps and notification wake turns
  (`crates/otto/src/app/mod.rs`), and reporting `idle` between them would make
  Herdr's sidebar flap. A duplicate `(state, message)` is not sent again.
- A sub-agent's approval request still pauses the session, so it reports
  `blocked`; other sub-agent activity does not change the state.
- Automatic (AUTO) decisions never reach a human, so a request that is
  resolved without waiting is at most a transient `blocked`. Herdr's own
  thresholds make that harmless.

### Where it lives

- New native module `crates/otto/src/herdr.rs`: environment gate, request
  encoding, a single background task owning the socket, and a latest-wins
  queue so reporting never blocks the agent loop. No `otto-core` change: the
  core stays WASM-safe and I/O-free.
- Wiring mirrors `auto_reflection::observing_prompt`
  (`crates/otto/src/app/mod.rs`), which already decorates a frontend
  `EventSink` per logical turn. The Herdr decorator wraps the same sink, so
  every frontend that runs a turn reports without knowing about Herdr.
- The reporter is constructed in the composition root (`cli::run`, next to the
  frontend selection) and released on exit, after the session closes.
- No configuration key. Reporting happens exactly when Herdr started the
  process; a user who does not want it runs Otto outside a Herdr pane. `source`
  is fixed to `custom:otto` and the label to `otto`: changing either needs a
  real use, not a setting.
- Capability rule: this adds no tool, so the
  [capability descriptions](../development.md#capability-descriptions)
  obligations do not apply.

## Compatibility and safety

- Outside Herdr nothing changes: no socket, no task, no new log line, no new
  startup work.
- The report carries ids, state and the session path only — no prompt text,
  no tool arguments, no tool output, no credentials. The `message` field is
  the approval title, which is already shown in the TUI and persisted in the
  approval journal.
- Reporting is one-way: Otto writes requests and reads only the reply line it
  needs to know the write landed. It never asks Herdr for state.
- Session storage, the wire protocol, the Pi v3 format and existing frontends
  are unchanged; no new provider, no new model behavior.

## Testing

Offline, no live Herdr and no live provider:

- Unit: the environment gate (absent `HERDR_ENV`, `HERDR_PANE_ID` or
  `HERDR_SOCKET_PATH` ⇒ no connection attempt), monotonic `seq`, dedupe of an
  unchanged `(state, message)`, request encoding against the `herdr api
  schema` field names.
- Integration (`crates/otto/tests/`): a `tokio::net::UnixListener` on a
  `TempDir` path standing in for Herdr. It asserts that starting the frontend
  claims the pane first (`report_agent(idle)` → `report_agent_session` with the
  session id and path) and then the sequence `report_agent(working)` →
  `report_agent(blocked, message)` → `report_agent(working)` →
  `report_agent(idle)` for a turn with a human-decided approval, plus
  `release_agent` on exit, and that a missing or closed socket changes nothing
  observable in the run.
- The test does not start a Herdr session; the protocol fixture is the schema
  exported by `herdr api schema`.

Acceptance on a real machine (manual, in the user's Herdr session):

1. `herdr pane split --current --direction right --cwd "$PWD"` and run `otto`
   in the new pane: `herdr agent list` shows `otto` as soon as the TUI is up,
   `working` while a prompt runs and `idle` after it.
2. An approval prompt makes `herdr agent wait <pane> --until blocked` return,
   and the sidebar marks the pane.
3. `herdr agent prompt <pane> "…"` submits the text to the TUI.
4. Quitting Otto leaves no agent row behind.

## Documentation

- User manual: a short "Running Otto inside Herdr" section — start Otto in a
  pane, what the states mean, and that Herdr does not detect Otto itself (use
  a pane command, not `herdr agent start`).
- CHANGELOG: one `Unreleased` entry describing the behavior and the limits.
- Dev guide: no new contract beyond this document.

## Limitations

- Otto's TUI draws on the terminal's alternate screen, so Herdr's
  `agent read` only sees the current viewport. Herdr's own guidance for that
  case (ask the agent to write a file) applies; reporting cannot fix it.
- Herdr restores agents through the session reference, and its documentation
  describes restore for the bundled integrations. Herdr 0.9.3 accepts and
  stores a custom-reported reference (verified 2026-10-07 in a live pane), but
  whether it *restores* one after a server restart is still unverified, so
  `resume_argv` is reported best-effort and no Otto behavior depends on it.

## Decisions

Resolved 2026-10-07 by the maintainer:

1. **No configuration key** — the Herdr environment is the only switch.
2. **The reported label is always `otto`.**
3. **The first release covers Otto started directly in the pane** (local TUI and
   REPL). `otto serve`, `otto --attach` and `otto acp` are out of scope.
4. **The session reference and `resume_argv` are reported in the first
   release**, best-effort.
5. **Otto reports only; it does not name itself** (2026-10-07). The panel and
   `herdr agent list` show the label `otto`, and agent commands target the pane;
   a user who wants the name alias runs `herdr agent rename <pane> otto`.

No open questions remain.
