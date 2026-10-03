# Memory review notifications

**Status:** Approved for implementation (2026-10-03)

## Goal

When a pending memory candidate is created, an interactive Otto frontend must
prompt a human to review it without requiring them to remember `/memory` or
`/memory review` first. This improves discovery only: a model still cannot
approve, reject, or otherwise mutate a candidate.

## Existing behavior

- `remember` and `forget` proposals, reflection, and external callers write
  pending candidates through the native memory service.
- The REPL and TUI can list and decide them with `/memory review`.
- The connector already refreshes Telegram/Feishu Approve/Deny cards after a
  successful model turn for that chat session. It does not learn about a
  candidate created outside that turn, including an API `remember` call.

## Contract

### Event

The native memory service publishes an in-process `memory_review_available`
generation after a successful operation creates a pending candidate. The signal
contains only a monotonically increasing integer: it has no candidate id,
action, scope, origin, text, reason, labels, metadata, credentials, prompt,
tool arguments, or raw request.

The event is best effort and not persisted. It is a discovery signal, not the
source of review truth. A missed event is recovered by the existing
`/memory review` command or a later local TUI refresh.

The event is emitted only after the SQLite operation succeeds and only for a
candidate in `pending` state. Human `/remember` writes records directly and
emit no review event.

### Local REPL

The REPL prints one bounded, content-free line after receiving the event:

```text
Memory review available. Run /memory review.
```

It does not interrupt a running command or write a candidate decision.

### TUI

The local TUI receives the event while idle or during a turn. It queries only
its current controller's existing user/workspace scopes and presents a modal
with action, kind, and key; candidate body and reason are deliberately absent.
The modal supports Up/Down to select, `a` to accept, `r` to reject, and Esc to
close without deciding.

A decision uses the same shared `/memory review <id> accept|reject` command
path as the REPL. No modal action is automatic. If a candidate cannot be read,
the TUI shows a safe unavailable status and retains normal command recovery.

### Connector and attached frontends

Telegram/Feishu retain their existing post-turn memory cards. This change does
not add an ACP notification extension or broadcast memory events to connector,
attached TUI, web, or server clients: those are separate processes and require
a distinct authenticated transport design. Their existing `/memory` recovery
path remains available.

## Ownership

- `crates/otto/src/memory`: generation-only notification publisher; SQLite
  remains the review truth.
- `crates/otto/src/app`, `crates/otto/src/cli`, and `crates/otto/src/tui`:
  local frontend subscription, fixed hint, and modal using existing review
  command logic.

`otto-core` remains wasm-safe and contains neither process-local notification
state nor native UI/connector dependencies.

## Safety and privacy

- Review is always human initiated; events grant no authority.
- Candidate values are not placed in event streams, logs, metrics, errors, or
  ordinary availability notices. The authorized local review modal obtains
  details only through its existing scoped memory query.
- Existing scope/session checks remain mandatory before read or decision.
- The event queue is bounded and drops oldest signals under load. The durable
  candidate store and explicit commands provide recovery.
- No configuration format, provider support, token, OAuth, or secret handling
  changes.

## Acceptance

1. A successful tool/reflection/API proposal changes only a content-free local
   generation; it never exposes candidate text or reason through the signal.
2. Local REPL output has the fixed availability hint.
3. A local TUI proposal opens a scoped modal without a manual `/memory` command;
   Esc leaves the candidate pending and `a`/`r` use the existing review result.
4. Missed, stale, duplicate, unavailable, and restart cases fail safely and are
   recoverable with existing review commands.
5. Tests prove no candidate text or secret-bearing request content enters event
   payloads or modal state.
