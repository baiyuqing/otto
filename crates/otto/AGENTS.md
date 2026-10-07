# Native runtime

The [repository rules](../../AGENTS.md) apply. Start with the
[ownership map](../../docs/code-organization.md#cratesotto-native-application)
and [package contracts](../../docs/development.md#package-boundaries).

- `runtime/startup.rs` is the startup composition root; `cli::run`,
  `otto serve` and `embed` call its phases. `runtime` assembles agents;
  `app` owns shared lifecycle. CLI/TUI/ACP/server/client consume services;
  services and `embed` never import frontends. `tests/runtime_boundary.rs`
  enforces this.
- `otto::embed` is public API and changes only by addition; see the
  [embedding API contract](../../docs/development.md#embedding-api).
- Native `config` owns source discovery, executable trust and the backed-up
  compare-and-swap writer. `tool/project_guard.rs` rechecks authorization at
  effects. Provider identity/credentials and security grants never come from
  project preference sources; each workspace resolves its own overlay.
- Use `fsops` and workspace directory handles for confined/atomic operations.
  Keep native session storage append-only and Pi-compatible.
- Memory callers use `memory::Service`, not the store directly. Reflection
  queues human-reviewed candidates; its vetted skill writer and boundary
  tests remain the authority for filesystem mutation.
- Bash uses `sandbox`. macOS Seatbelt is the supported confined driver;
  Linux has no confined driver and bash stays unavailable unless the user
  explicitly selects `--sandbox off`. Keep platform code behind `cfg`, and
  limit claims to the platform actually tested.
- External MCP/provider transports, OAuth, Turso, scheduling, failover and
  native process control remain in this crate, outside the WASM-safe core.
- Run adjacent behavioral tests and relevant architecture guards, then the
  root `make check-fast`. macOS-only acceptance cannot be validated by a
  Linux run that discovers zero tests.

Detailed reflection, memory, ACP, task and storage contracts live in the
linked development guide; keep one canonical contract rather than copying
inventories into agent instructions.
