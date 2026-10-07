# YOLO session approval mode

Extends [session automatic approval](2026-10-05-session-auto-mode.md) and
[authority boundaries](2026-10-07-auto-mode-authority-boundaries.md).

Approved scope: 2026-10-07.

## Problem

The session policy has two modes. MANUAL decides nothing; AUTO decides only a
bounded authorization that stays inside the sandbox and the workspace. Every
other class — a plan review, a sandbox escalation, a cross-workspace delivery,
an external MCP call, a persistent read grant, a scheduled unattended run, a
memory, skill or profile change — waits for the user in either mode by
design, because a human judgement, a boundary crossing and durable authority
are not the session policy's to decide on its own.

That rule protects a user who is watching. It gives no answer to a user who
has decided, for one session, that they want the work to continue without
them: the only lever available is answering each card, which is the
interruption they are trying to remove. There is no session-scoped way to say
"this session decides everything, I am accountable for it".

## Contract

A session has one of three persisted modes.

| Mode | Decides without the user |
| --- | --- |
| `manual` | nothing |
| `auto` (default) | a bounded authorization: `authority == bounded && kind == authorization && persistent == false` |
| `yolo` | every request whose scope carries this session's id, whatever its authority class, kind or persistence |

`ApprovalMode::decides(&Request)` is the single policy predicate:

```text
manual => false
auto   => request.auto_decidable()
yolo   => true
```

YOLO is a user decision, not a widening of AUTO: it reaches the classes the
authority contract reserves for a person, which is exactly what the user
selected. The classes keep their meaning — a request still declares
`consent`, `boundary` or `durable`, and the recorded decision still names the
mode that decided it — so an audit can tell an automatic bounded decision from
a blanket one.

Enforcement points, all of which must agree:

1. `otto::approval::Service` decides nothing outside `ApprovalMode::decides`
   for the session's persisted mode, so `create`, `wait` and the
   `set_mode` sweep of pending requests follow one rule.
2. The commit fence rechecks the persisted mode and the policy predicate
   inside the committing transaction, so a stale decision cannot publish after
   the mode changed. The decision transport is `auto-mode` in AUTO and
   `yolo-mode` in YOLO.
3. Only an admitted user action sets the mode. `ApprovalMode` never appears in
   a model tool schema or tool argument; the model cannot read or write it.
4. Scope rules are unchanged: a request without a session id (a workspace-wide
   request) is never decided by a session policy, in any mode.

YOLO extends the session's reach to its own child tasks: a request whose scope
carries this session id and a non-empty `task_id` is decided under YOLO, while
AUTO keeps asking (a child task does not inherit AUTO authority). A request
belonging to another session is never decided by this session's mode.

## Compatibility

- MANUAL and AUTO keep their exact meaning, storage rows and API behavior.
  A row persisted before this change decodes as `auto` or `manual` from its
  stored boolean.
- `approvals.db` moves to schema version 3: `approval_modes` stores `mode`
  (`manual`/`auto`/`yolo`) instead of the `automatic` boolean. An older
  database is rewritten in place, preserving every row's meaning, inside the
  same transaction that opens it. A newer version is still refused without
  modification.
- The logical approval migration record keeps decoding a payload written by
  the boolean form and writes the mode form; the offline migration journal
  therefore survives an interrupted cross-backend migration.
- The mode APIs stay backward compatible and gain the mode:
  `GET|PUT /v1/sessions/{id}/approval-mode` accepts `{"automatic":bool}` and
  `{"mode":"manual"|"auto"|"yolo"}` and returns `{"automatic":bool,"mode":...}`;
  ACP `_otto/approval_mode/get|set` takes `automatic` or `mode` and answers
  the same shape. `_meta.otto.approvalMode.version` stays `1`.
- The embedding API keeps `set_auto_mode(bool)` and gains
  `set_approval_mode(ApprovalMode)`; `Info` reports the mode.
- No Telegram/Feishu enabling surface is added: `/auto on|off` keeps its
  meaning there. The connector displays the mode it reads instead of showing
  YOLO as AUTO.

## Consequences

- In YOLO a request is created, decided and applied in one flow: the model's
  `request_approval` returns an approved decision without a dialog, a sandbox
  escalation runs, a persistent grant is written, a scheduled `prompt` item is
  accepted, and a memory, skill or profile change is applied. Each is still
  journalled, and the user can read the events afterwards.
- Approval is still not execution: a failed or unknown application is reported
  as such, and a rejected/withdrawn request stays rejected.
- The mode is visible in the TUI/Web footer (`YOLO`), in `/auto` and in
  `Info`; `/yolo off` is one command away and takes effect for pending requests
  as well.
- A user who enables YOLO accepts that a wrong request is approved without
  being seen. The spec deliberately does not add a hidden exception.

## Rejected alternatives

- **Make AUTO decide everything.** It would silently widen the meaning of
  every existing session's stored preference, including sessions that never
  opted in.
- **A process-only mode that resets on session load.** Considered and not
  chosen: the persisted form is what the commit fence rechecks inside the
  committing transaction, and a mode that silently resets is harder to reason
  about than one the frontends display.

## Follow-ups

- `approvals_reviewer = auto_review` (from the authority-boundaries spec): the
  middle ground between AUTO and YOLO — a reviewer agent for boundary requests
  with a deny policy and a circuit breaker.
- An optional expiry or an explicit grant count for YOLO, so unattended runs
  end by themselves.
