# The otto-terminal crate

The [repository rules](../../AGENTS.md) apply. Use the
[ownership map](../../docs/code-organization.md#repository-map) and
[package contracts](../../docs/development/architecture.md#package-boundaries).

- Scope: the terminal frontend. `tui` is the full-screen TUI, local and
  `--attach`; `cli` holds `login`, `repl` and `repl_commands` (the line
  REPL, the slash commands and the login flows), which `tui` also uses.
  Entry points for `otto-cli`: `tui::run_with_observer_bindings`,
  `tui::attach`, `cli::repl`, `cli::login`.
- Dependencies: `otto`, `otto-core` and `otto-client`. This crate never
  imports `otto-server`, `otto-acp` or `otto-cli`, and `otto-server` never
  imports it.
- Domain behavior belongs in `otto` (`app` is the shared lifecycle). A
  presenter or command here calls it and does not reimplement it. Keep the
  dependency directions inside `tui` that `tests/tui_boundary.rs` checks
  (interaction kernel, feature presenters, rendering).
- Unit tests sit beside the modules. Run them with
  `cargo test -p otto-terminal`. Scaffolding comes from `otto::fixtures`
  through the `test-support` dev-dependency. The PTY tests are in
  `crates/otto-cli/tests/` (`tui_pty`, `tui_attach_pty`, `make test-tui`).
- Code files stay at 800 lines and 200 characters per line; a new module is
  not named `utils`, `helpers`, `common`, `misc`, `types` or `config`.
