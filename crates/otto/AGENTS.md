# Native runtime

The [repository rules](../../AGENTS.md) apply. Start with the
[ownership map](../../docs/code-organization.md#cratesotto-native-runtime-library)
and [package contracts](../../docs/development/architecture.md#package-boundaries).

- This crate is a library. It keeps the runtime, tools and `fixtures`. The
  process-wide primitives (`fsops`, `log`, `retry`, `deadline`, `gourl`,
  `urlprivacy` and the other platform modules) live in
  [otto-platform](../otto-platform/AGENTS.md), which `otto` imports as
  `otto_platform::...` and does not re-export. The SQL layer and its SQLite
  driver live in [otto-storage](../otto-storage/AGENTS.md), imported as
  `otto_storage::...` and not re-exported; domain schemas and repositories stay
  in each domain's `sqlite.rs`. The fault vocabulary and the doubles that need
  no `otto` module live in [otto-faults](../otto-faults/AGENTS.md), imported as
  `otto_faults::...`; only `faults/mcp.rs` and `faults/sandbox.rs` stay here. The
  `otto serve` client, the terminal UI, the HTTP server, the ACP frontends
  and the `otto` binary live in [otto-client](../otto-client/AGENTS.md),
  [otto-terminal](../otto-terminal/AGENTS.md),
  [otto-server](../otto-server/AGENTS.md), [otto-acp](../otto-acp/AGENTS.md)
  and [otto-cli](../otto-cli/AGENTS.md); dependencies run one way and none of
  them is imported by `otto`.
- `runtime/startup.rs` is the startup composition root; `otto_cli::run`
  and `otto serve` call its phases. `runtime` assembles agents;
  `app` owns shared lifecycle.
- Top-level modules belong to the layers in
  [Layers inside `otto`](../../docs/development/architecture.md#layers-inside-otto):
  persistence, capability, orchestration, assembly (`runtime`),
  application (`app`), frontend. Import only the same or a lower layer, with
  no cycle inside a layer; move a needed item down instead of importing up.
  A new module gets a layer in the same PR. `tests/layer_boundary.rs`
  enforces this.
- The public API is what the frontend crates need: widen an item from
  `pub(crate)` to `pub` when one calls it, and put new frontend code into
  the frontend crates, not here.
- A test-only item that a dependent crate's tests need is `pub` and gated
  `#[cfg(any(test, feature = "test-support"))]`, not `#[cfg(test)]`.
  Shared scaffolding lives in `otto::fixtures`.
- Native `config` owns source discovery, executable trust and the backed-up
  compare-and-swap writer. `tool/project_guard.rs` rechecks authorization at
  effects. Provider identity/credentials and security grants never come from
  project preference sources; each workspace resolves its own overlay.
- Use `otto_platform::fsops` and workspace directory handles for confined/atomic operations.
  Keep native session storage append-only and Pi-compatible.
- Memory callers use `memory::Service` for the built-in domain or
  `memory::client::Client` for the neutral service, not a store directly.
- Bash uses `sandbox`. macOS Seatbelt is the supported confined driver;
  Linux has no confined driver and bash stays unavailable unless the user
  explicitly selects `--sandbox off`. Keep platform code behind `cfg`, and
  limit claims to the platform actually tested.
- External MCP/provider transports, OAuth, SQLite, scheduling, failover and
  native process control remain in this crate, outside the I/O-free core.
- Add or update adjacent behavioral tests and architecture guards. macOS-only
  acceptance cannot be validated by a Linux run that discovers zero tests.

Detailed memory, ACP, task and storage contracts live in the linked development
guide; keep one canonical contract rather than copying inventories into agent
instructions.
