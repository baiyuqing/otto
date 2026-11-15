# The otto-client crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: the HTTP client of `otto serve` over its Unix socket
  (`otto_client::Client`, `Error`, `TurnStream`, `StatusStream`). One module
  per route family. The contract is in the `src/lib.rs` module comment.
- Dependencies: `otto` and `otto-core` only. This crate never imports
  `otto-server`, `otto-terminal`, `otto-acp` or `otto-cli`. `otto-terminal`
  (TUI `--attach`) and `otto-acp` (`--attach` relay) import it.
- A new `otto serve` route that a frontend calls gets its client call here;
  the route itself belongs to [otto-server](../otto-server/AGENTS.md).
- Unit tests sit in `src/tests.rs` and `src/tests/` and call an in-process
  router. Run them with `cargo test -p otto-client`. Scaffolding comes from
  `otto::fixtures` through the `test-support` dev-dependency.
