# The otto-eval crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[testing guide](../../docs/development/testing.md#memory-retraction-evaluation).

- Scope: the development evaluations behind `otto eval` (`eval_command` for
  parsing, dispatch and shared infrastructure; `memory_eval_command` for the
  `memory-retraction` target). A new evaluation target belongs here, not in
  `otto-cli`.
- Dependencies: `otto` and `otto-core` only. This crate never imports
  `otto-server`, `otto-acp` or `otto-cli`. `otto-cli` links it only with its
  `eval` feature, off by default, so nothing the default `otto` binary needs
  may live here.
- Evaluations read no session, change no session and register no product
  tool. Every text they write passes through the runtime's redactor.
- Embedded data comes from `testdata/memory-eval/` through `include_str!`
  relative to `crates/otto-eval/src/memory_eval_command/`.
- Unit tests sit beside the modules. Scaffolding comes from `otto::fixtures`
  through the `test-support` dev-dependency.
- Test with `cargo test -p otto-eval`. Run the command with
  `cargo run -p otto-cli --features eval -- eval memory-retraction --dry-run`.
