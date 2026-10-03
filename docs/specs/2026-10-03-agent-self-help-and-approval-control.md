# Agent self-help and pending approval control

Approved scope: installed-version help, current-session capabilities and pending
approval query/withdrawal, and a restricted conversation while approval waits.

## Ownership

`tool::otto` owns `otto_help`, `approval_pending` and `approval_revoke`.
Help reads the canonical user manual embedded in the installed executable;
its capabilities topic uses the actual parent runner definitions. HTTP and
ACP remain user/client APIs, not implicitly callable model tools.

`app::approval_control` owns the restricted conversation. It reuses the
session provider, model, thinking setting, redaction and usage collection, but
uses an isolated memory transcript with only help, pending query, withdrawal
and queue tools. No Bash, file, delegation, grant or configuration tools are
available. Dialogue outcomes enter the original runner's inbox; only that
runner writes its append-only transcript.

Ordinary tasks are returned to the frontend's existing queue. The original
approval and its task keep waiting. The control dialogue is bounded to 30
seconds and bound to the session and original approval ID. A later request
cannot be withdrawn by an earlier dialogue.

## Withdrawal

Withdrawal removes only an exact request still awaiting a human decision.
Unknown, replaced, reserved by an accepted decision, granted or consumed
requests fail without changing other state.
The approval state publishes a change so HTTP waits finish with a deny event
and local ACP withdraws the permission request using `$/cancel_request`.
Existing card and dialog handling updates attached clients. Model withdrawal
never approves or undoes executed commands or permanent grants.

Approval configuration writes and tool withdrawals share a decision mutex so
persistent read or Always grants cannot be changed concurrently by withdrawal.
HTTP/ACP reserve an accepted Allow before waking the retry loop, so a later
model withdrawal cannot overtake that decision.

## Entry points

- HTTP: `POST /v1/sessions/{id}/approvals/message`, body `{"text":"..."}`.
- ACP extension: `_otto/approvals/message`, parameters `sessionId` and `text`.
  Advertised by `_meta.otto.approvalDialogue`, including attach mode.
- Responses: `null` when there is no ungranted approval, otherwise
  `{"text":"...","queued":false}` (or true for a normal queued task).
- Telegram/Feishu send ordinary input through this extension while a card
  waits. Clients of other ACP agents fall back to queueing and explicit
  `/deny` or `/stop`.
- TUI `c` dismisses the approval panel without deciding, allowing chat input.

## Verification

Offline tests cover embedded help, current tool lists, invalid and foreign
request IDs, read approvals, granted requests, waiter notification, restricted
provider tools, dialogue withdrawal and queueing, and client protocol routing.
Canonical gates are `make check-fast`, `make check`, and `make connect-check`.
