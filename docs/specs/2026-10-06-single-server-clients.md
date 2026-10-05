# One server, multiple clients and workspaces

Status: proposed; interface changes below require user approval.

## Goal and trust boundary

One `otto serve` owns sessions in admitted workspaces. Web, TUI, HTTP and ACP
clients belong to the same trusted user. A server token or Unix socket grants
server-wide authority; this proposal does not add multi-user accounts or ACLs.
Separate sessions keep separate histories, queues and events. Explicitly
loading the same session shares conversation and approvals. Background session
replies continue to reach every admitted chat bound to that session.

## ACP workspace selection

- Local `otto acp` keeps its existing one-workspace contract.
- ACP attached to `serve`, including direct TCP, forwards each `session/new`
  and `session/load` request's absolute `cwd` to the HTTP session API.
- `session/list` uses its requested `cwd`; omitted `cwd` uses the connection's
  startup workspace for compatibility.
- Admission remains in `serve`: canonical directory validation, configured
  roots and explicit trust. ACP does not implicitly trust a directory.
- Session ids retain their workspace identity. A mismatching resume fails.
  Prompts and cancellations use the registered session and turn ids, not the
  connection's startup directory.
- No new process is required for direct ACP TCP clients. A connector's
  configured workspace continues to select the `cwd` it sends.

## Scheduled chat delivery

- Connectors advertise their admitted destinations in
  `clientCapabilities._meta.otto.scheduleDeliveryChats`, an array of
  `<platform>:<chat-id>` strings. Existing platform capabilities remain.
- Exact destinations scope delivery claims before a connector acquires a
  lease. A connector cannot claim an item belonging to another destination.
- For clients without the new capability, keep platform delivery only when
  no other connection offers that platform. With overlapping legacy clients,
  suspend ambiguous delivery and explain the conflict; do not send the body
  to a client whose destinations are unknown.
- The connector checks its allowlist again at send time. Lease/retry and
  acknowledgement semantics remain unchanged; successful delivery is not
  guaranteed exactly once across an unknown external send outcome.

## Acceptance

Offline tests cover simultaneous ACP sessions in two admitted workspaces,
rejection of unadmitted or mismatching workspaces, cancellation confined to
the originating turn, separate notification subscriptions, exact scheduled
destination claims and ambiguous legacy delivery. Existing shared-session
behavior and local ACP compatibility must keep passing.

## Existing bug fixes

The accompanying implementation fixes workspace matching on resume, per-
workspace runtime/sandbox composition, read-grant wiring, subscription rebinding
and connector allowlist enforcement. These restore existing contracts and do
not depend on approval of the interface changes above.
