# Active-task guidance: TODO and progress

Canonical design: [active-task guidance](2026-10-04-active-task-guidance.md).
Issue: [#305](https://github.com/baiyuqing/otto/issues/305).
Branch: `baiyuqing/active-task-guidance`.
Worktree: primary checkout `.worktree/active-task-guidance`.
Last updated: 2026-10-04. No implementation gate has passed yet.

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

## Implementation

- [x] **Core implemented and package-tested:** transient steering mailbox, generation binding,
  safe-point delivery, atomic final close, cancellation/drop cleanup, and tests.
- [x] **Core implemented and package-tested:** model-originated follow-up input preserving
  `Role::Context` / `agent_followup` provenance.
- [x] **Core implemented and package-tested:** failed ACP-visible tool completion on terminal
  persistence failure, with an injected-failure regression test.
- [ ] **In progress — native owner:** controller steering/control dialogue,
  isolated tools, captured active-task context, 30-second bound, serialized input.
- [ ] **In progress — native owner:** real `task_queue` tool, bounded shared
  follow-up execution after the logical turn/approval, cancellation cleanup.
- [ ] **Native owner:** durable capability policy and accurate registered schemas;
  request-capture tests after compaction, reload, and approval retry.
- [ ] **In progress — protocol owner:** HTTP routes/client operations and OpenAPI
  fixtures for control operations.
- [ ] **In progress — protocol owner:** ACP local/attach/TCP operations and
  capability negotiation; optional
  `/steer` and `/queue` commands with bounded local follow-up admission.
- [x] **Connector implemented and focused-tested:** immediate natural-message routing and
  explicit commands; reply
  isolation, queue limits, session generation, and old-agent fallback.
- [ ] Telegram shortcut entries, user manual, and relevant navigation docs.

## Acceptance evidence

Check an item only after its behavior is implemented and verified. Record the
test/command and result here; static inspection does not count as runtime proof.

- [ ] Guidance during a blocked provider/tool, FIFO ordering, exact-turn final
  completion race, no cross-turn leakage, cancellation/failure cleanup.
- [ ] Idle/approval rejection, full queue/invalid input, session isolation.
- [ ] Natural guidance vs follow-up routing, model tool effects, no-decision/error
  behavior, stale control dialogue, and no approval bypass.
- [ ] Model-originated follow-up provenance, execution order, no automatic loop
  beyond the budget, and cleanup across approval/cancellation.
- [ ] Direct terminal prompt pending-approval boundary: no early execution or
  silent follow-up loss; persisted visible notice and accurate tool contract.
- [ ] Capability instructions and callable schemas in real requests after history
  compaction, restored session, and approval retry; disabled capabilities absent.
- [ ] Default configuration exposes tools, persistent capability instructions,
  natural-message routing, and command discovery without enable flags.
- [ ] ACP local, attached server, direct TCP, and connector end-to-end coverage.
- [ ] Exactly one terminal tool update on success, active-tool cancellation,
  approval denial, execution failure, and persistence failure.
- [ ] Focused Rust/Go tests.
- [ ] `make check-fast`.
- [ ] `make connect-check`.
- [ ] Full `make check` on supported macOS host.
- [ ] Integration review, PR creation, CI reporting, and issue progress links.

### Verified so far

- Core owner: `cargo test -p otto-core --lib` passed **579/579**; focused guidance,
  cancellation cleanup, agent-source follow-up, terminal persistence failure,
  and existing Inbox input tests passed. This is core behavior proof, not ACP
  or connector end-to-end proof.
- Core owner: `cargo check -p otto-core --target wasm32-unknown-unknown` passed.
- Core owner: formatting of its three modified files passed. Whole-workspace
  formatting and the full gates are pending while other files are being edited.
- Connector owner: `go test ./internal/agent` and focused bridge routing tests
  passed (guidance/follow-up, unsupported capability, error/null responses,
  queue bounds, late control reply after stop, approval preservation). Full
  Go race and actual Otto/connector end-to-end gates are pending.

## Deferred

Structured ACP plans/usage, fork/resume/pagination, durable queued-task recovery,
and changes to ordinary TUI input semantics are outside this first delivery.
Existing task notifications/reminders retain their persistent Inbox behavior.
Continuous follow-up resumption across direct CLI/TUI approval prompts is also
deferred; those callers must report and clear follow-ups that cannot run yet.
