# The otto executable

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: the `otto` binary (`[[bin]] name = "otto"`, `src/main.rs`) and the
  composition root `otto_cli::run` in `src/run.rs`. It owns command parsing,
  flags, subcommands (`serve`, `acp`, `config`, `setup`, `plugin`, `eval`,
  `storage`, `workflow`, ...) and process lifecycle.
- Dependencies are one-way: this crate is last in the chain
  `otto-core` <- `otto` <- `otto-server` <- `otto-acp` <- `otto-cli` and
  imports all four. No other crate imports it.
- Domain behavior belongs in `otto`, HTTP in `otto-server`, ACP in
  `otto-acp`. A command module calls them; it does not reimplement them.
- `cli::{login, repl, repl_commands}` stay in `otto` because `tui` uses them.
- Unit tests sit beside the modules. Integration tests are in `tests/`: the
  binary, ACP, serve, signal, PTY, version and plugin CLI
  tests, plus `binary_harness/`.
  Run one with `cargo test -p otto-cli --test <name>`.
- `tests/version.rs` checks the shared version in `Cargo.lock` for `otto`,
  `otto-core`, `otto-server`, `otto-acp` and `otto-cli`.
- Scaffolding comes from `otto::fixtures` through the `test-support`
  dev-dependency. A test-only item in `otto` that this crate needs is public
  and gated `#[cfg(any(test, feature = "test-support"))]`.
- Build with `cargo build -p otto-cli` or `cargo build`.
