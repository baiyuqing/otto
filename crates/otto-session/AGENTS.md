# The otto-session crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: session files and workspace identity (not the confined `Workspace`,
  which is `otto::workspace`). The crate root (`src/lib.rs`)
  holds the JSONL session `Store`, prepared sessions (`prepared.rs`), listing,
  fork, archive and delete, the `SessionRepository` contract and its POSIX
  implementation (`posix_repository.rs`), the writer lease (`lease/`) and the
  `origin` of a turn. `workspace.rs` and `workspace/` hold the workspace
  identity marker, its move history and admission.
- Session history stays append-only and compatible (see the repository
  rules); the on-disk layout and the lease files are read by other Otto
  processes. File access goes through `otto_platform::fsops`.
- Dependencies: `otto-core` and `otto-platform` only. Code that needs a
  provider, a tool, the runtime, SQL or any `otto` module does not belong here;
  `otto` imports this crate as `otto_session::...` and does not re-export it.
  `otto-faults` is a dev-dependency for unit and integration tests; production
  source must not name `otto_faults`.
- The `test-support` feature adds test accessors (`lease::Lease::for_test`,
  `lease::without_acquire_wait`, `Store::hold_state_for_test`,
  `Store::read_header`, `Lease::mark_lost_for_test`). It is off in a shipped build; the `test-support`
  feature of `otto` turns it on.
- Tests: unit tests sit beside the code (`src/tests/`, `src/lease/*tests.rs`,
  `workspace/` test modules); `tests/fault_session.rs` runs the store on
  fault doubles; `tests/storage_boundary.rs` guards against SQL, storage
  imports and `otto_core` re-exports. Run them with `cargo test -p otto-session`.
