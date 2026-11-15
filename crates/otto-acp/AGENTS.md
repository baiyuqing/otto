# ACP frontends

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: ACP v1 over stdio and TCP (`serve`) and the `--attach` relay to a
  running `otto serve` (`attach.rs`). Entry point: `src/lib.rs`.
- Dependencies are one-way: `otto-core` <- `otto` <- `otto-server`, and
  `otto` <- `otto-client`, both <- `otto-acp`. This crate imports `otto`,
  `otto-server`, `otto-client` and `otto-core`. It never imports `otto-cli`
  or `otto-terminal`. Command-line parsing and process lifecycle belong
  in `otto-cli`.
- Session lifecycle, approvals, task control and notices come from
  `otto::app`; the attach relay talks to `otto serve` through `otto_client`.
  Do not reimplement them here.
- Tests live beside the sources (`channel_tests.rs` and others). Run them with
  `cargo test -p otto-acp`. Scaffolding comes from `otto::fixtures` through
  the `test-support` dev-dependency.
- A test-only item in `otto` that this crate needs is public and gated
  `#[cfg(any(test, feature = "test-support"))]`.
- The stdio and attach process tests are in `crates/otto-cli/tests/`
  (`acp_stdio`, `acp_attach`); add cases to the scenario module that owns
  them.
- `chat-connector/` is an independent Go ACP client and links no code from
  this crate.
