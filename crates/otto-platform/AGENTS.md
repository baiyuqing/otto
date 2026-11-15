# The otto-platform crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: process-wide primitives with no domain knowledge: `hex`, `deadline`,
  `retry`, `fsops` (pinned POSIX file driver and the `FileDriver` contract),
  `log`, `tls_provider`, `urlprivacy`, `sse` (the Server-Sent Events parser
  used by the MCP HTTP transport and by `otto-client`), `program_status`,
  `herdr`, `random`, `child_fence` and `process_identity`. `gourl` is private: it is the URL
  parser behind `urlprivacy`, and other crates call `urlprivacy`.
- Dependencies: `otto-core` only. This crate never imports `otto` or a crate
  above it. A new module here must not name a domain concept of `otto`
  (session, workspace, tool, memory); such code stays in `otto`.
- `otto` and the frontend crates import these modules as
  `otto_platform::<module>`. `otto` does not re-export them.
- A module that only `otto` uses still becomes `pub` here; keep the surface to
  what a dependent crate calls. A test-only item that a dependent crate's tests
  need is `pub` and gated `#[cfg(any(test, feature = "test-support"))]`.
- Unit tests sit next to the code (`gourl/tests.rs`, `fsops/`, `#[cfg(test)]`
  modules). Run them with `cargo test -p otto-platform`.
