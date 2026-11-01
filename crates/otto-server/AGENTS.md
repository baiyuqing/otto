# HTTP serve frontend

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: the `otto serve` HTTP/SSE frontend: routes, listeners, TLS,
  authentication middleware, workspaces, tasks, timers, workflows and
  schedule routes. Entry point: `src/lib.rs` (`otto_server::Server`).
- Dependencies are one-way: `otto-core` <- `otto` <- `otto-server`. This
  crate imports `otto` and `otto-core` only. It never imports `otto-acp` or
  `otto-cli`. Process startup and flags belong in `otto-cli`.
- Behavior shared with other frontends belongs in `otto::app`, not here. If
  this crate needs an `otto` item that is `pub(crate)`, widen it to `pub`;
  do not copy domain behavior.
- HTTP/SSE DTOs stay in this crate, separate from internal types. Share only
  wire types that cross frontends, from `otto-core`.
- Tests live in `src/tests/` and `src/tests.rs`. Run them with
  `cargo test -p otto-server`. Scaffolding comes from `otto::fixtures` and
  `otto::faults` through the `test-support` dev-dependency; do not duplicate
  provider doubles or temporary controllers here.
- A test-only item in `otto` that this crate needs is public and gated
  `#[cfg(any(test, feature = "test-support"))]`.
- The process-level `otto serve` tests are in `crates/otto-cli/tests/`
  (`serve_listen`).
