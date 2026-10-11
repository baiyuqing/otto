# Shared core

The [repository rules](../../AGENTS.md) apply. This crate owns shared pure
contracts and stays buildable for `wasm32-unknown-unknown`.

- Keep filesystem, process/environment access, network transports, credentials,
  Turso and native sandbox integration in `crates/otto`.
- Own neutral model/provider/tool/approval contracts, the agent loop, Pi v3
  codecs, wire DTOs, configuration resolution and secret-safe text here.
  Provider wire formats stay in `openaicompat` or `openairesponses`.
- `config/layered.rs` owns raw presence-preserving merge, source authority,
  provenance and managed constraints. Native callers supply I/O and trust.
  Never serialize merged/defaulted configuration back over an owning source.
- In-place config edits use `config::edit` and must reject changes beyond the
  intended key/table while preserving supported comments and layout.
- Shared contract changes keep native/browser consumers consistent; the host
  tests and `make rust-wasm-check` cover both.

See [package boundaries](../../docs/development/architecture.md#package-boundaries) for
module ownership and [the architecture design](../../docs/specs/2026-09-05-architecture-contracts.md)
for compatibility rationale.
