# Active-task guidance: TODO and progress

Canonical design: [active-task guidance](2026-10-04-active-task-guidance.md).
Issue: [#305](https://github.com/baiyuqing/otto/issues/305).
Branch: `baiyuqing/active-task-guidance`.
Worktree: primary checkout `.worktree/active-task-guidance`.
Last updated: 2026-10-04. Implementation and full host acceptance passed after
integrating main at `8593f0c` (#306). PR publication and CI status are pending.

## Completed investigation

- [x] Confirm natural conversation and durable model discoverability are required;
  commands alone do not satisfy the task.
- [x] Trace existing agent Inbox, Controller admission, server FIFO, ACP local/
  attach, connector queues, approval dialogue, and tool event mapping.
- [x] Identify Inbox completion race; choose turn-bound transient guidance.
- [x] Identify tool-state gap when terminal-fact persistence fails after start.
- [x] Confirm existing `agent_send` for parent-to-child guidance; reuse it.
- [x] Record approved requirements and scope in the design document.
- [x] Record the user's requirement: requested features enabled by default;
  no default-off switch or extra opt-in setup in this delivery.
- [x] Record authorization: create the PR automatically after implementation and
  required checks, then link it here and in #305.

## Implementation

- [x] **Core implemented and package-tested:** transient steering mailbox, generation binding,
  safe-point delivery, atomic final close, cancellation/drop cleanup, and tests.
- [x] **Core implemented and package-tested:** model-originated follow-up input preserving
  `Role::Context` / `agent_followup` provenance.
- [x] **Core implemented and package-tested:** failed ACP-visible tool completion on terminal
  persistence failure, with an injected-failure regression test.
- [x] **Native implemented and focused-tested:** controller steering/control dialogue,
  isolated tools, captured active-task context, 30-second bound, serialized input.
- [x] **Native implemented and focused-tested:** real `task_queue` tool, bounded shared
  follow-up execution after the logical turn/approval, cancellation cleanup.
- [x] **Native implemented and request-tested:** durable capability policy and accurate registered schemas;
  request-capture tests after compaction, reload, and approval retry.
- [x] **Protocol implemented and focused-tested:** HTTP routes/client operations and OpenAPI
  fixtures for control operations.
- [x] **Protocol implemented and focused-tested:** ACP local/attach/TCP operations and
  capability negotiation; optional
  `/steer` and `/queue` commands with bounded local follow-up admission.
- [x] **Connector implemented and focused-tested:** immediate natural-message routing and
  explicit commands; reply
  isolation, queue limits, session generation, and old-agent fallback.
- [x] Telegram shortcut entries, user manual, and relevant navigation docs.

## Acceptance evidence

Check an item only after its behavior is implemented and verified. Record the
test/command and result here; static inspection does not count as runtime proof.

- [x] Guidance during a blocked provider/tool, FIFO ordering, exact-turn final
  completion race, no cross-turn leakage, cancellation/failure cleanup.
- [x] Idle/approval rejection, full queue/invalid input, session isolation.
- [x] Natural guidance vs follow-up routing, model tool effects, no-decision/error
  behavior, stale control dialogue, and no approval bypass.
- [x] Model-originated follow-up provenance, execution order, no automatic loop
  beyond the budget, and cleanup across approval/cancellation.
- [x] Direct terminal prompt pending-approval boundary: no early execution or
  silent follow-up loss; persisted visible notice and accurate tool contract.
- [x] Capability instructions and callable schemas in real requests after history
  compaction, restored session, and approval retry; disabled capabilities absent.
- [x] Default configuration exposes tools, persistent capability instructions,
  natural-message routing, and command discovery without enable flags.
- [x] ACP local, attached server, direct TCP, and connector end-to-end coverage.
- [x] Exactly one terminal tool update on success, active-tool cancellation,
  approval denial, execution failure, and persistence failure.
- [x] Focused Rust/Go tests.
- [x] `make check-fast` (passed as the first stage of the host `make check`).
- [x] `make connect-check`.
- [x] Full `make check` on supported macOS host.
- [ ] PR creation, CI reporting, and issue progress links (authorized; next step).

### Verification evidence

- Final host `make check` passed at `03d8c24` after merging #306: formatting,
  workspace Clippy, **580 core tests**, **2,210 native unit tests**, full Rust
  integration/ACP tests, Go vet/race with real Otto connector end-to-end tests,
  wasm builds/tests, scripts, PTY tests, and **125 UI tests**.
- Real macOS approval tests passed for local and attached ACP, including one-time
  elevation, persistent read grants, denial, and late/revoked responses.
- Captured provider requests retain actual `task_queue` schemas/policy before
  and after compaction, restored sessions, and approval retries.
- Actual model tool -> queued follow-up HTTP execution verifies ordering,
  separated answers, and agent provenance. Shared approval tests verify the
  follow-up scope survives approval and denial discards pending model work.
- Natural control tests cover steer/queue/read-only choices, exact original user
  text, restricted tool schemas, and accepted effects despite a failed final
  control-model reply. Connector tests cover legacy fallbacks, limits, FIFO,
  approval isolation, stop invalidating late replies, and command discovery.
- Tool-state tests verify exactly one terminal update for completed, denied,
  and actively cancelled calls in local/attached ACP; injected persistence
  failure closes the display as unknown failure. Live/replay use the same
  disposition mapping.
- Logs: `/tmp/otto-active-task-merged-check.log` (final full gate),
  `/tmp/otto-approval-capability-tests.log`, and
  `/tmp/otto-followup-approval-test.log` (focused proofs before the final gate).

### Integration review follow-ups

- [x] Approval decision cleanup must bind the original session/request even if
  the controller closes or changes during the callback. Hold admission through
  applying an Allow and admitting its retry; cover lifecycle races with tests.
- [x] A new ACP proxy attached to an old server must translate missing control
  routes into an unsupported-method response. The connector must take its
  existing fallback without submitting `/steer` as a model prompt.
- [x] Preserve FIFO order of queued steering commands; removed the unnecessary
  priority insertion and verified the connector race/regression tests.
- [x] Update default-tool/prompt regression expectations to include `task_queue`;
  both focused tests pass. The first full native run passed 2,186 tests and
  failed only these two outdated expectations; the final full gate passed after updating them.
- [x] Actual active-tool cancellation exposed `completed` display status despite
  a cancelled operation. Live/replay ACP mapping now uses disposition; both local
  and attached active-tool cancellation tests pass with exactly one failed update.
- [x] Native approval lifecycle regressions pass for replacement, close, busy
  admission, and duplicate request IDs; read-grant/rollback/approve-always checks
  pass. Final host acceptance passed the real permission path.

## Deferred

Structured ACP plans/usage, fork/resume/pagination, durable queued-task recovery,
and changes to ordinary TUI input semantics are outside this first delivery.
Existing task notifications/reminders retain their persistent Inbox behavior.
Continuous follow-up resumption across direct CLI/TUI approval prompts is also
deferred; those callers must report and clear follow-ups that cannot run yet.
