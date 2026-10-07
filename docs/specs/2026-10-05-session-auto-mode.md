# Session automatic approval

Authority boundaries: [AUTO mode authority boundaries](2026-10-07-auto-mode-authority-boundaries.md)
limits automatic decisions to a bounded authorization; the rules below about
which request the policy may decide are superseded by that contract.

Approved scope: 2026-10-05. Current behavior is documented in the user manual.
This extends the unified approval contract: approvals may be decided by the
user-selected session policy, as well as by an explicit user decision.

## Contract

- Session requests default to AUTO. `/auto` reads the mode; `/auto on|off`
  selects AUTO or MANUAL. The command is a user control, never a model tool.
  AUTO decides only a request that is a bounded authorization: inside the
  sandbox and the workspace, kind `authorization`, not persistent. Every other
  class stays a user decision in either mode.
- `approval_modes` in `approvals.db` persists the preference by exact workspace
  and session ID. New sessions use AUTO; loading a session preserves its
  preference. Different sessions do not share preferences.
- Workspace-only requests and independent child tasks do not inherit the
  parent session's automatic authority.
- Creation records a Pending event, then the AUTO policy records Approved
  with decision transport `auto-mode` when the request is auto-decidable.
  Manual decisions and automatic decisions
  use the same validation and journal. Switching to AUTO approves existing
  pending auto-decidable requests owned by this session, not merely visible
  shared requests.
- Automatic decisions preserve argument/version validation, cancellation,
  application receipts, sandbox behavior and recovery rules. Approval does
  not mean execution succeeded. Failed or unknown application is shown as such.
- Frontends show request details and automatic approval, with a distinct mode
  indicator in TUI/Web status bars. Chat notifications read durable event
  changes, so a request that is immediately approved cannot disappear unseen.

## APIs

HTTP (under existing server admission/authentication):

- `GET /v1/sessions/{id}/approval-mode` -> `{"automatic":true}`.
- `PUT /v1/sessions/{id}/approval-mode`, body `{"automatic":false}` -> saved mode.
- `GET /v1/sessions/{id}/approval-events?after=<cursor>` -> up to 100 ordered
  approval changes, each `{sequence,request}`. Advance the cursor only after
  consuming each change. Existing approval SSE remains available.

ACP extensions (local stdio, attach and authenticated loopback TCP):

- `_otto/approval_mode/get`, params `{"sessionId":"..."}`.
- `_otto/approval_mode/set`, params `{"sessionId":"...","automatic":false}`.
- Both return `{"automatic":boolean}`. The agent advertises
  `_meta.otto.approvalMode.version=1`.
- `_otto/approvals/watch` returns the same bounded JSON event changes in local
  and attach mode. The attach relay uses the JSON session event endpoint.

Telegram and Feishu route `/auto [on|off]` to the mode API immediately,
including while a task waits for approval. Telegram publishes `/auto` in its
command menu and offers AUTO/MANUAL inline buttons. Each button carries the
exact session ID; admission still checks chat/sender, and a stale button
cannot configure the chat's replacement session.
