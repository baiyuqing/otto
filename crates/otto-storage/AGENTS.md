# The otto-storage crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: the SQL layer over SQLite. `lib.rs` holds `Connection`, `Statement`,
  `Rows`, `Transaction`, `OptionalExtension` and the `params!` macro; `sql.rs`
  holds values, errors and the `SqlDriver`, `SqlConnection`, `SqlStatement` and
  `SqlRows` traits; `worker.rs` is the bounded async adapter worker;
  `schema_objects.rs` compares schema objects; `drivers/sqlite*` is the
  `rusqlite` driver.
- Out of scope: domain schemas, SQL text and repositories. They stay in each
  domain module of `otto` as `sqlite.rs` or `sqlite/` files, and
  `crates/otto/tests/storage_boundary.rs` checks that only those files hold SQL
  or import the driver. The fault-injecting driver `otto::faults::sql` stays in
  `otto`.
- Dependencies: third-party crates only (`rusqlite`, `libc`, `thiserror`,
  `tokio`); no workspace crate. This crate never imports `otto` or a domain
  concept. `rusqlite` is a dependency of this crate alone; `otto` does not
  import it.
- `otto` and the crates above it import items as `otto_storage::<item>` and
  call the macro as `otto_storage::params!`. `otto` does not re-export them.
- An item that a dependent crate calls is `pub`; keep the rest `pub(crate)`. A
  test-only item that a dependent crate's tests need is `pub` and gated
  `#[cfg(any(test, feature = "test-support"))]`.
- Unit tests sit next to the code (`#[cfg(test)]` modules, `*_tests.rs`). Run
  them with `cargo test -p otto-storage`.
