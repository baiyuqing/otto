# Native runtime

The [repository rules](../../AGENTS.md) apply. Start with the
[ownership map](../../docs/code-organization.md#cratesotto-native-runtime-library)
and [package contracts](../../docs/development/architecture.md#package-boundaries).

- This crate is a library. It keeps the runtime, tools, `tui`, `client`,
  `embed`, `cli::{login, repl, repl_commands}` (used by `tui`) and
  `fixtures`. The HTTP server, ACP frontends and the `otto` binary live in
  [otto-server](../otto-server/AGENTS.md), [otto-acp](../otto-acp/AGENTS.md)
  and [otto-cli](../otto-cli/AGENTS.md); dependencies run one way,
  `otto-core` <- `otto` <- `otto-server` <- `otto-acp` <- `otto-cli`.
- `runtime/startup.rs` is the startup composition root; `otto_cli::run`,
  `otto serve` and `embed` call its phases. `runtime` assembles agents;
  `app` owns shared lifecycle. Services and `embed` never import frontends.
  `tests/runtime_boundary.rs` enforces this.
- The public API is what the frontend crates need: widen an item from
  `pub(crate)` to `pub` when one calls it, and put new frontend code into
  the frontend crates, not here.
- A test-only item that a dependent crate's tests need is `pub` and gated
  `#[cfg(any(test, feature = "test-support"))]`, not `#[cfg(test)]`.
  Shared scaffolding lives in `otto::fixtures`.
- `otto::embed` is public API and changes only by addition; see the
  [embedding API contract](../../docs/development/architecture.md#embedding-api).
- Native `config` owns source discovery, executable trust and the backed-up
  compare-and-swap writer. `tool/project_guard.rs` rechecks authorization at
  effects. Provider identity/credentials and security grants never come from
  project preference sources; each workspace resolves its own overlay.
- Use `fsops` and workspace directory handles for confined/atomic operations.
  Keep native session storage append-only and Pi-compatible.
- Memory callers use `memory::Service` for the built-in domain or
  `memory::client::Client` for the neutral service, not a store directly.
- Bash uses `sandbox`. macOS Seatbelt is the supported confined driver;
  Linux has no confined driver and bash stays unavailable unless the user
  explicitly selects `--sandbox off`. Keep platform code behind `cfg`, and
  limit claims to the platform actually tested.
- External MCP/provider transports, OAuth, SQLite, scheduling, failover and
  native process control remain in this crate, outside the WASM-safe core.
- Add or update adjacent behavioral tests and architecture guards. macOS-only
  acceptance cannot be validated by a Linux run that discovers zero tests.

Detailed memory, ACP, task and storage contracts live in the linked development
guide; keep one canonical contract rather than copying inventories into agent
instructions.
