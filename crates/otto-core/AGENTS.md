# Core

The [repository rules](../../AGENTS.md) apply. This crate holds I/O-free
contracts, formats and the agent loop, and stays buildable for
`wasm32-unknown-unknown`. `crates/otto` holds their implementations; the
[layer map](../../docs/code-organization.md#rust-layers) pairs each contract
with its implementation.

- Code belongs here when it performs no I/O and is a contract (trait or type),
  a format (Pi v3 session, provider wire formats in `openaicompat` or
  `openairesponses`, wire DTOs, configuration schema and layered merge) or
  part of the agent loop. A domain service that implements a contract stays
  in `crates/otto` even where a part of it performs no I/O, for example
  approval policy and memory validation.
- Filesystem, process, environment, network, stdio and clock access,
  credentials, Turso and sandbox integration stay in `crates/otto`.
  `tests/io_boundary.rs` fails on `std::fs`, `std::process`, `std::net`,
  `std::env`, stdio and `now()` calls outside `#[cfg(test)]`;
  `make rust-wasm-check` does not detect them because they compile on wasm32.
- `crates/otto` imports these items as `otto_core::...` and does not
  re-export them, so each item has one path;
  `crates/otto/tests/storage_boundary.rs` enforces it.
- `config/layered.rs` owns raw presence-preserving merge, source authority,
  provenance and managed constraints. Native callers supply I/O and trust.
  Never serialize merged/defaulted configuration back over an owning source.
- In-place config edits use `config::edit` and must reject changes beyond the
  intended key/table while preserving supported comments and layout.
- A contract change updates its `crates/otto` implementations in the same
  change; `make rust-wasm-check` keeps the crate buildable for wasm32.

See [package boundaries](../../docs/development/architecture.md#package-boundaries) for
module ownership and [the architecture design](../../docs/specs/2026-09-05-architecture-contracts.md)
for compatibility rationale.
