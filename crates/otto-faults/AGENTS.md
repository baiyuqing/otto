# The otto-faults crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: fault-injection doubles for the contracts of `otto-platform` and
  `otto-storage`. `plan.rs` holds the fault vocabulary (`FaultPlan`, `Fault`,
  `Rule`, `Trigger`, `Filter`); `files.rs` and `publish.rs` wrap the `fsops`
  file driver and publish operations; `sql.rs` wraps the `SqlDriver`;
  `crash.rs` ends a process at a named operation; `http.rs` is the scripted
  loopback HTTP origin server.
- Test support only. No shipped build links this crate: `otto` enables it
  through its `fault-injection` feature and a dev-dependency, and other crates
  use it as a dev-dependency. Production source must not name `otto_faults`;
  `crates/otto/tests/faults_boundary.rs` scans every crate's `src`.
- Out of scope: a double that needs an `otto` domain module (`mcp`, `sandbox`)
  stays in `otto::faults`. This crate depends on `otto-platform` and
  `otto-storage` and never on `otto`.
- Callers import items as `otto_faults::<item>` (the fault vocabulary at the
  crate root, doubles as `otto_faults::files::...`). `otto` does not
  re-export them.
- Unit tests sit next to the code (`#[cfg(test)]` modules). Run them with
  `cargo test -p otto-faults`.
