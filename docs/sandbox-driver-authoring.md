# Sandbox Driver Authoring

Stage 1 ships two driver modes only:

- `seatbelt` on macOS
- `off` for explicit unsafe direct execution

Docker, Apple Container, Podman, and remote backends are planned only.

## Driver contract

A driver must implement the exact `internal/sandbox.Driver` contract.

Required behavior:

- defensively copy request slices before starting work;
- never retain request slices, contexts, or stream writers after `Execute` returns;
- enforce policy all-or-nothing before the command starts;
- write stdout and stderr only to the provided streams;
- report ordinary nonzero exits in `ExitStatus`, not as transport errors;
- return truthful capabilities for read confinement, write confinement, network allow/deny, and Unix-socket denial;
- make `Close` idempotent, reject new work after close starts, and clean up driver-owned descendants before returning.

## Safety rules

- Return bounded safe errors only. Do not expose control-plane credentials, provider endpoints, private profile paths, raw host diagnostics, auth headers, or cookie values.
- Child cleanup must cover normal exit, cancellation, timeout, and concurrent close to the backend's documented guarantee.
- Do not partially start a command when the requested policy cannot be enforced.

## Verification

A new driver must pass:

- `sandboxtest.RunDriverContract`
- capability-specific filesystem tests
- capability-specific network tests
- close/cancellation/descendant-cleanup tests
- safe-error and redaction boundary tests

## Forbidden host integrations

A future container or VM driver must not:

- mount or forward Docker/Podman sockets;
- mount or forward SSH/GPG agents;
- mount the host home directory by default;
- auto-build or trust project-owned Dockerfiles;
- use mutable image tags as trusted defaults.

Use pinned trusted images/base artifacts, explicit mounts, and explicit conformance evidence instead.
